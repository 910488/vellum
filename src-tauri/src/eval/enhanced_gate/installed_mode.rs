//! Installed-Desktop qualification.
//!
//! The bridge gate proves the bridge is correct. It cannot prove that the app
//! on the user's machine goes through it — that is a property of the packaged
//! Codex Desktop, the per-user `CODEX_CLI_PATH` lease, and the launch the user
//! actually performs. This gate restarts the real packaged app through Vellum's
//! managed restart and refuses to call anything a success until the bridge that
//! belongs to that launch id attests it is ready with both children up.
//!
//! Two deliberate scope notes, so the report is not read as more than it is:
//!
//! * Vellum cannot drive Codex Desktop's own UI, so the Official control task
//!   and the third-party smoke task are created by driving the *installed*
//!   configuration — same manifest, same binaries, same trusted model map, same
//!   durable binding database — through a gate-owned bridge process. Desktop's
//!   adoption of the bridge is proven separately, by the attestation.
//! * Those tasks create threads and read back their durable bindings. They do
//!   not run turns against live providers; spending a user's quota belongs to
//!   the live promotion lane, not to a gate that must be safe to run.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::enhanced_runtime::launch_manifest::{LaunchManifestV1, LAUNCH_MANIFEST_ENV};
use crate::enhanced_runtime::qualification::QualificationResult;
use crate::enhanced_runtime::{
    adopted_by_codex_desktop, BridgeAttestationV1, ExecutionPlane, ThreadRuntimeBindingStore,
    TrustedModelProviderMap,
};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

use super::{value_after, BinaryIdentity, CaseResult, GateMode, GateReport, ObservedBinding};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Reported verbatim when the packaged app never showed up on our bridge.
pub const BRIDGE_NOT_OBSERVED: &str = "EnhancedDesktopBridgeNotObserved";
pub const ACTIVE_TURN_REFUSAL: &str = "EnhancedInstalledGateActiveTurn";

/// CLI entry point.
pub async fn run(run_id: &str, run_root: &Path, args: &[String]) -> AppResult<GateReport> {
    let state = AppState::try_new().map_err(|error| {
        AppError::Message(format!(
            "cannot initialize Vellum state for the installed gate: {error}"
        ))
    })?;
    // A CLI process cannot see whether a turn is running inside Codex Desktop.
    // Rather than assume the safe answer, it makes the operator say so.
    let acknowledged_idle = args.iter().any(|argument| argument == "--no-active-turn");
    execute(run_id, run_root, &state, acknowledged_idle, args).await
}

/// Settings-button entry point. Here the active-turn check is a real reading.
pub async fn run_from_desktop(state: &AppState) -> AppResult<QualificationResult> {
    let data_root = state.data_root();
    let run_id = super::new_run_id(GateMode::Installed);
    let run_root = data_root
        .join("enhanced-runtime/qualifications")
        .join(&run_id);
    std::fs::create_dir_all(&run_root)
        .map_err(|error| AppError::Message(format!("create {}: {error}", run_root.display())))?;
    let report = execute(&run_id, &run_root, state, state.active_requests() == 0, &[]).await?;
    let path = super::write_report(&run_root, &report)?;
    let result = QualificationResult {
        run_id: report.run_id.clone(),
        mode: report.mode.clone(),
        started_at: report.started_at,
        finished_at: report.finished_at,
        passed: report.passed,
        promotion_ready: report.promotion_ready,
        report_path: path,
        failures: report.failures.clone(),
    };
    result
        .store(&data_root)
        .map_err(|error| AppError::Message(format!("store qualification result: {error}")))?;
    Ok(result)
}

async fn execute(
    run_id: &str,
    run_root: &Path,
    state: &AppState,
    acknowledged_idle: bool,
    args: &[String],
) -> AppResult<GateReport> {
    let started_at = chrono::Utc::now().timestamp();
    let mut report = GateReport::new(run_id.to_string(), GateMode::Installed, started_at);
    let data_root = state.data_root();

    if !acknowledged_idle {
        report.record(CaseResult::new(
            "no-active-turn",
            false,
            format!(
                "{ACTIVE_TURN_REFUSAL}: a turn may be in flight; finish it and re-run (CLI: pass --no-active-turn once you have confirmed Codex is idle)"
            ),
        ));
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    }
    report.record(CaseResult::new(
        "no-active-turn",
        true,
        "no turn is in flight",
    ));

    // The same four checks `vellum-eval enhanced-runtime-status` prints. They
    // live in one place on purpose: a machine the preflight refuses must not be
    // able to produce a green qualification report by taking a different route
    // into the same state.
    let preflight = super::preflight::evaluate(&data_root);
    let status = preflight.status.clone();
    let preflight_passed = preflight.passed();
    for check in preflight.checks {
        report.record(check);
    }
    if !preflight_passed {
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    }
    report.bridge = status.bridge_executable.as_deref().map(BinaryIdentity::of);
    report.official = status
        .official_codex_executable
        .as_deref()
        .map(BinaryIdentity::of);
    report.enhanced = status
        .enhanced_codex_executable
        .as_deref()
        .map(BinaryIdentity::of);

    // The bridge is armed only while the Proxy is serving — that rule is what
    // stops `CODEX_CLI_PATH` from pointing at a takeover with no data plane
    // behind it. A CLI process starts with no Proxy, so without this the
    // launch silently disarms and the gate measures a plain Codex restart
    // while reporting on Enhanced.
    let proxy = match crate::commands::proxy::start_proxy_inner(state).await {
        Ok(status) => status,
        Err(error) => {
            report.record(CaseResult::new(
                "the-proxy-is-serving-so-the-bridge-can-be-armed",
                false,
                format!("the Proxy would not start, so Enhanced cannot be armed: {error}"),
            ));
            report.finish(chrono::Utc::now().timestamp());
            return Ok(report);
        }
    };
    report.record(CaseResult::new(
        "the-proxy-is-serving-so-the-bridge-can-be-armed",
        proxy.running,
        format!("proxy running={}", proxy.running),
    ));
    if !proxy.running {
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    }

    // 1. Managed restart of the real packaged app.
    let first = crate::commands::runtime::restart_codex_managed(state, false).await?;
    let Some(attestation) = record_restart(&mut report, &first, "first", &status) else {
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    };
    let manifest = LaunchManifestV1::read(&LaunchManifestV1::path_in(&data_root))
        .map_err(|error| AppError::Message(error.to_string()))?;
    report.launch_id = Some(manifest.launch_id.clone());
    report.attestation_state = Some(attestation.state.as_str().to_string());
    report.bridge_pid = Some(attestation.bridge_pid);
    report.official_child_pid = attestation.official.pid;
    report.enhanced_child_pid = attestation.enhanced.pid;
    report.enhanced_identity = attestation.enhanced_identity.clone();
    report.session_features_applied = attestation.session_features_applied;

    let model_map_matches =
        attestation.model_provider_map_sha256 == manifest.model_provider_map_sha256;
    let children_present = attestation.official.pid.is_some() && attestation.enhanced.pid.is_some();
    report.record(CaseResult::new(
        "attestation-names-both-children-and-the-launch-model-map",
        model_map_matches && children_present,
        format!(
            "official={:?} enhanced={:?} modelMapMatches={model_map_matches}",
            attestation.official.pid, attestation.enhanced.pid
        ),
    ));

    // 2. Official control task and third-party smoke task against the installed
    //    configuration, read back from the durable binding database.
    let map = TrustedModelProviderMap::read(&manifest.model_provider_map_path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let official_model = pick_model(&map, &manifest.official_provider_ids);
    let third_party_model = pick_model(&map, &manifest.third_party_provider_ids);
    let (Some(official_model), Some(third_party_model)) = (official_model, third_party_model)
    else {
        report.record(CaseResult::new(
            "installed-catalog-covers-both-planes",
            false,
            "the trusted model map has no Official or no third-party catalog id",
        ));
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    };

    let bridge_executable = status
        .bridge_executable
        .clone()
        .ok_or_else(|| AppError::Message("no packaged bridge executable is configured".into()))?;
    let extra_timeout = value_after(args, "--task-timeout-seconds")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(REQUEST_TIMEOUT);

    let manifest_path = LaunchManifestV1::path_in(&data_root);
    let probe_manifest_path = write_probe_manifest(run_root, &manifest, "first")?;
    let probe_attestation_path = run_root.join("first-bridge-attestation.json");
    let threads = tokio::task::spawn_blocking({
        let bridge_executable = bridge_executable.clone();
        let manifest_path = probe_manifest_path;
        let official_model = official_model.clone();
        let third_party_model = third_party_model.clone();
        move || {
            let mut client =
                InstalledBridgeClient::start(&bridge_executable, &manifest_path, extra_timeout)?;
            client.initialize()?;
            let official = client.start_thread(&official_model)?;
            let third_party = client.start_thread(&third_party_model)?;
            client.shutdown();
            Ok::<_, AppError>((official, third_party))
        }
    })
    .await
    .map_err(|error| AppError::Message(format!("installed task runner panicked: {error}")))?;

    let (official_thread, third_party_thread) = match threads {
        Ok(threads) => threads,
        Err(error) => {
            report.record(CaseResult::new(
                "installed-configuration-serves-both-planes",
                false,
                error.to_string(),
            ));
            report.finish(chrono::Utc::now().timestamp());
            return Ok(report);
        }
    };

    let session_features_applied = BridgeAttestationV1::read(&probe_attestation_path)
        .map(|attestation| attestation.session_features_applied)
        .unwrap_or(0);
    report.session_features_applied = report
        .session_features_applied
        .saturating_add(session_features_applied);
    report.record(CaseResult::new(
        "third-party-session-applies-the-attested-feature-profile",
        session_features_applied > 0,
        format!("sessionFeaturesApplied={session_features_applied}"),
    ));

    let store = ThreadRuntimeBindingStore::open(&manifest.binding_db)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let official_binding = store
        .get(&official_thread)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let third_party_binding = store
        .get(&third_party_thread)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let routed = official_binding
        .as_ref()
        .is_some_and(|binding| binding.plane == ExecutionPlane::OfficialCodex)
        && third_party_binding.as_ref().is_some_and(|binding| {
            binding.plane == ExecutionPlane::EnhancedCodex
                && binding.runtime_digest == manifest.enhanced.runtime_digest
        });
    report.record(CaseResult::new(
        "official-control-task-and-third-party-smoke-task-route-correctly",
        routed,
        format!(
            "official={} thirdParty={}",
            official_binding
                .as_ref()
                .map(|binding| binding.plane.as_str())
                .unwrap_or("<unbound>"),
            third_party_binding
                .as_ref()
                .map(|binding| binding.plane.as_str())
                .unwrap_or("<unbound>")
        ),
    ));
    report.bindings = [official_binding, third_party_binding.clone()]
        .into_iter()
        .flatten()
        .map(|binding| ObservedBinding::from_binding(&binding))
        .collect();

    // 3. Restart Desktop again and resume the third-party task on a new launch.
    let second = crate::commands::runtime::restart_codex_managed(state, false).await?;
    let Some(second_attestation) = record_restart(&mut report, &second, "second", &status) else {
        report.finish(chrono::Utc::now().timestamp());
        return Ok(report);
    };
    let reloaded = LaunchManifestV1::read(&manifest_path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let resume_probe_manifest_path = write_probe_manifest(run_root, &reloaded, "resume")?;
    let resumed = tokio::task::spawn_blocking({
        let manifest_path = resume_probe_manifest_path;
        let thread = third_party_thread.clone();
        move || {
            let mut client =
                InstalledBridgeClient::start(&bridge_executable, &manifest_path, extra_timeout)?;
            client.initialize()?;
            let response = client.request("thread/resume", json!({"threadId": thread}))?;
            client.shutdown();
            Ok::<_, AppError>(response)
        }
    })
    .await
    .map_err(|error| AppError::Message(format!("installed resume runner panicked: {error}")))?;

    let resume_ok = match resumed {
        Ok(response) => {
            let same_thread = response
                .pointer("/result/thread/id")
                .and_then(Value::as_str)
                == Some(third_party_thread.as_str());
            let binding = store
                .get(&third_party_thread)
                .ok()
                .flatten()
                .filter(|binding| {
                    binding.plane == ExecutionPlane::EnhancedCodex
                        && binding.runtime_digest == reloaded.enhanced.runtime_digest
                });
            same_thread && binding.is_some() && second_attestation.enhanced.pid.is_some()
        }
        Err(_) => false,
    };
    report.record(CaseResult::new(
        "resume-after-a-desktop-restart-keeps-the-same-enhanced-digest",
        resume_ok,
        format!(
            "digest={} launch={}",
            reloaded.enhanced.runtime_digest, reloaded.launch_id
        ),
    ));

    report.finish(chrono::Utc::now().timestamp());
    Ok(report)
}

fn write_probe_manifest(
    run_root: &Path,
    source: &LaunchManifestV1,
    label: &str,
) -> AppResult<PathBuf> {
    let mut probe = source.clone();
    probe.attestation_path = run_root.join(format!("{label}-bridge-attestation.json"));
    probe.qualification_journal_path = run_root.join(format!("{label}-qualification.jsonl"));
    let path = run_root.join(format!("{label}-launch-manifest.json"));
    probe
        .write(&path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(path)
}

fn record_restart(
    report: &mut GateReport,
    outcome: &crate::commands::runtime::ManagedRestart,
    label: &str,
    status: &crate::enhanced_runtime::DesktopRuntimeStatus,
) -> Option<BridgeAttestationV1> {
    let attestation = outcome.attestation.clone();
    let adopted = attestation.as_ref().is_some_and(adopted_by_codex_desktop);
    // A restart that writes a new launch manifest while Codex Desktop keeps
    // the bridge it already had produces exactly this: a fresh launch id, a
    // live attestation, both children up — and every turn served by a binary
    // this build replaced. Nothing in the attestation says which bridge wrote
    // it, so the process has to be asked directly.
    let running_bridge = attestation.as_ref().and_then(|attestation| {
        crate::enhanced_runtime::process_info::executable_of(attestation.bridge_pid)
    });
    let bridge_matches = match (&running_bridge, &status.bridge_executable) {
        (Some(running), Some(configured)) => same_path(running, configured),
        _ => false,
    };
    // Recorded whenever an attestation exists, not only when adoption also
    // succeeded: "which binary is actually serving" is a separate fact from
    // "who started it", and reading a green report should never require
    // knowing which of the two failed.
    if attestation.is_some() && !bridge_matches {
        report.record(CaseResult::new(
            &format!("{label}-restart-lands-on-the-configured-bridge"),
            false,
            format!(
                "{BRIDGE_NOT_OBSERVED}: the live bridge runs from {}, not {}",
                running_bridge
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unknown>".into()),
                status
                    .bridge_executable
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unset>".into())
            ),
        ));
    }
    let observed = outcome.restarted && adopted && bridge_matches;
    report.record(CaseResult::new(
        &format!("{label}-managed-restart-is-observed-on-the-bridge"),
        observed,
        if observed {
            format!(
                "launch={} bridgePid={}",
                outcome
                    .launch
                    .as_ref()
                    .map(|launch| launch.launch_id.clone())
                    .unwrap_or_default(),
                attestation
                    .as_ref()
                    .map(|value| value.bridge_pid)
                    .unwrap_or(0)
            )
        } else {
            format!(
                "{BRIDGE_NOT_OBSERVED}: notice={} adoptedByDesktop={adopted}",
                outcome.notice.code
            )
        },
    ));
    observed.then_some(attestation).flatten()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

fn pick_model(map: &TrustedModelProviderMap, providers: &[String]) -> Option<String> {
    map.models
        .iter()
        .find(|(_, route)| providers.iter().any(|id| id == &route.provider_id))
        .map(|(model, _)| model.clone())
}

/// Minimal App Server client used only to create and resume threads against the
/// installed configuration.
struct InstalledBridgeClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    buffered: Vec<Value>,
    next_id: i64,
    timeout: Duration,
}

impl InstalledBridgeClient {
    fn start(bridge: &Path, manifest_path: &PathBuf, timeout: Duration) -> Result<Self, AppError> {
        let mut child = Command::new(bridge)
            .arg("app-server")
            .env(LAUNCH_MANIFEST_ENV, manifest_path)
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
            timeout,
        })
    }

    fn initialize(&mut self) -> Result<(), AppError> {
        let response = self.request(
            "initialize",
            json!({"clientInfo": {
                "name": "vellum-installed-gate",
                "title": "Vellum Installed Gate",
                "version": env!("CARGO_PKG_VERSION")
            }}),
        )?;
        if response.get("error").is_some() {
            return Err(AppError::Message(format!(
                "installed bridge initialize failed: {}",
                response
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown initialize error")
            )));
        }
        self.send(&json!({"method": "initialized"}))
    }

    fn send(&mut self, value: &Value) -> Result<(), AppError> {
        serde_json::to_writer(&mut self.stdin, value)
            .map_err(|error| AppError::Message(format!("write to bridge: {error}")))?;
        self.stdin
            .write_all(b"\n")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| AppError::Message(format!("flush bridge stdin: {error}")))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, AppError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            if let Some(index) = self
                .buffered
                .iter()
                .position(|value| value.get("id").and_then(Value::as_i64) == Some(id))
            {
                return Ok(self.buffered.remove(index));
            }
            match self.rx.recv_timeout(self.timeout) {
                Ok(value) => self.buffered.push(value),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AppError::Message(format!(
                        "the installed bridge did not answer {method} within {}s",
                        self.timeout.as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Message(
                        "the installed bridge closed its stdout".into(),
                    ))
                }
            }
        }
    }

    fn start_thread(&mut self, model: &str) -> Result<String, AppError> {
        let response = self.request("thread/start", json!({"model": model}))?;
        response
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                AppError::Message(format!(
                    "thread/start for {model} failed: {}",
                    response
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("no thread id")
                ))
            })
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::ModelProviderRoute;
    use std::collections::BTreeMap;

    fn map() -> TrustedModelProviderMap {
        let mut models = BTreeMap::new();
        models.insert(
            "gpt-control".to_string(),
            ModelProviderRoute {
                provider_id: "openai-official".into(),
                child_provider_id: "openai".into(),
            },
        );
        models.insert(
            "qwen-smoke".to_string(),
            ModelProviderRoute {
                provider_id: "qwen".into(),
                child_provider_id: "vellum".into(),
            },
        );
        TrustedModelProviderMap {
            schema_version: crate::enhanced_runtime::MODEL_PROVIDER_MAP_SCHEMA_VERSION,
            models,
        }
    }

    #[test]
    fn picks_one_catalog_id_per_plane_from_the_trusted_map() {
        let map = map();
        assert_eq!(
            pick_model(&map, &["openai-official".to_string()]),
            Some("gpt-control".to_string())
        );
        assert_eq!(
            pick_model(&map, &["qwen".to_string()]),
            Some("qwen-smoke".to_string())
        );
        assert_eq!(pick_model(&map, &["grok".to_string()]), None);
    }

    #[test]
    fn an_unobserved_bridge_is_never_reported_as_a_successful_restart() {
        let mut report = GateReport::new("run-1".into(), GateMode::Installed, 1);
        let outcome = crate::commands::runtime::ManagedRestart {
            restarted: false,
            notice: crate::model::RuntimeNotice::new("enhancedDesktopBridgeNotObserved"),
            launch: None,
            attestation: None,
        };
        assert!(
            record_restart(&mut report, &outcome, "first", &status_with_bridge(None)).is_none()
        );
        report.finish(2);
        assert!(!report.passed);
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains(BRIDGE_NOT_OBSERVED)));
    }

    #[test]
    fn a_restart_that_reports_success_without_desktop_adoption_still_fails() {
        let mut report = GateReport::new("run-1".into(), GateMode::Installed, 1);
        let mut attestation = BridgeAttestationV1 {
            schema_version: crate::enhanced_runtime::attestation::ATTESTATION_SCHEMA_VERSION,
            launch_id: "launch-1".into(),
            bridge_pid: std::process::id(),
            parent_pid: 1,
            parent_executable: Some(PathBuf::from("C:/tools/some-other-launcher.exe")),
            started_at: 1,
            updated_at: 1,
            state: crate::enhanced_runtime::BridgeLifecycle::Ready,
            official: Default::default(),
            enhanced: Default::default(),
            model_provider_map_sha256: "sha256:map".into(),
            binding_db: PathBuf::from("bindings.sqlite3"),
            binding_db_identity: "bindings.sqlite3#0".into(),
            enhanced_identity: None,
            session_features_applied: 0,
            open_turns: 0,
            failure_reason: None,
        };
        attestation.official.initialized = true;
        attestation.enhanced.initialized = true;
        let outcome = crate::commands::runtime::ManagedRestart {
            restarted: true,
            notice: crate::model::RuntimeNotice::new("enhancedDesktopBridgeReady"),
            launch: None,
            attestation: Some(attestation),
        };
        assert!(record_restart(
            &mut report,
            &outcome,
            "first",
            &status_with_bridge(Some(binary_of_this_process()))
        )
        .is_none());
        assert!(report
            .cases
            .iter()
            .any(|case| !case.passed && case.detail.contains(BRIDGE_NOT_OBSERVED)));
    }

    /// The failure the gate could not see: a restart that produced a live
    /// attestation, both children up, and a bridge binary from an older build.
    /// Nothing in the attestation names the executable, so without the process
    /// reading this report would have said the restart succeeded.
    #[test]
    fn a_restart_served_by_an_older_bridge_binary_is_not_a_successful_restart() {
        let mut report = GateReport::new("run-1".into(), GateMode::Installed, 1);
        let mut attestation = ready_attestation();
        attestation.parent_executable = Some(PathBuf::from(
            "C:/Program Files/WindowsApps/OpenAI.Codex_1/app/ChatGPT.exe",
        ));
        let outcome = crate::commands::runtime::ManagedRestart {
            restarted: true,
            notice: crate::model::RuntimeNotice::new("enhancedDesktopBridgeReady"),
            launch: None,
            attestation: Some(attestation),
        };
        let status = status_with_bridge(Some(PathBuf::from(
            "C:/vellum/binaries/dev/vellum-codex-app-server-d9ac.exe",
        )));
        assert!(record_restart(&mut report, &outcome, "first", &status).is_none());
        let case = report
            .cases
            .iter()
            .find(|case| case.name == "first-restart-lands-on-the-configured-bridge")
            .expect("the mismatched bridge binary is reported as its own case");
        assert!(!case.passed);
        assert!(case.detail.contains("vellum-codex-app-server-d9ac.exe"));
    }

    fn binary_of_this_process() -> PathBuf {
        std::env::current_exe().expect("the test binary has a path")
    }

    fn status_with_bridge(
        bridge: Option<PathBuf>,
    ) -> crate::enhanced_runtime::DesktopRuntimeStatus {
        let mut status = crate::enhanced_runtime::desktop_runtime_status(Path::new(
            "no-such-data-root-so-this-reads-as-unconfigured",
        ));
        status.bridge_executable = bridge;
        status
    }

    fn ready_attestation() -> BridgeAttestationV1 {
        let mut attestation = BridgeAttestationV1 {
            schema_version: crate::enhanced_runtime::attestation::ATTESTATION_SCHEMA_VERSION,
            launch_id: "launch-1".into(),
            bridge_pid: std::process::id(),
            parent_pid: 1,
            parent_executable: None,
            started_at: 1,
            updated_at: 1,
            state: crate::enhanced_runtime::BridgeLifecycle::Ready,
            official: Default::default(),
            enhanced: Default::default(),
            model_provider_map_sha256: "sha256:map".into(),
            binding_db: PathBuf::from("bindings.sqlite3"),
            binding_db_identity: "bindings.sqlite3#0".into(),
            enhanced_identity: None,
            session_features_applied: 0,
            open_turns: 0,
            failure_reason: None,
        };
        attestation.official.initialized = true;
        attestation.enhanced.initialized = true;
        attestation
    }
}
