//! Stdio App Server bridge used by Codex Desktop's `CODEX_CLI_PATH` hook.
//!
//! This module only multiplexes the native App Server protocol. It never
//! rewrites prompts, transcripts, tool calls, compaction, or turn semantics.
//!
//! It configures itself from the launch manifest on disk rather than from the
//! environment, because Codex Desktop is a packaged app: whoever starts it
//! does not get to hand it our variables. It also writes the attestation that
//! is the only evidence Vellum accepts for "Enhanced is actually running" —
//! the bridge is the one process that can see both children.

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;

use chrono::Utc;
use serde_json::{json, Value};
use vellum_enhanced_codex::{AblationProfile, EnhancedEventKind, EnhancedRuntimeFeatures};

use super::attestation::{AttestationWriter, BridgeAttestationV1, ChildAttestation};
use super::launch_manifest::{LaunchManifestError, LaunchManifestV1};
use super::observations::RuntimeObservations;
use super::qualification::{
    parse_enhanced_event, parse_enhanced_identity, QualificationJournal,
    ENHANCED_EVENT_NOTIFICATION, ENHANCED_IDENTITY_NOTIFICATION,
};
use super::{
    ExecutionPlane, ThreadRuntimeBinding, ThreadRuntimeBindingStore, TrustedModelProviderMap,
    TrustedProviderSet,
};

#[path = "multiplex.rs"]
mod multiplex;

/// Set on each child so an Enhanced fork can report its own identity back
/// without guessing which plane it was started as.
const PLANE_ENV: &str = "VELLUM_EXECUTION_PLANE";
const DIGEST_ENV: &str = "VELLUM_RUNTIME_DIGEST";
const FEATURE_PROFILE_ENV: &str = "VELLUM_ENHANCED_FEATURE_PROFILE";
/// Compatibility selector consumed by the session hook loader in every
/// Enhanced Core artifact published before the effective-profile contract.
const ABLATION_PROFILE_ENV: &str = "VELLUM_ENHANCED_ABLATION_PROFILE";
const ENHANCED_COMMIT_ENV: &str = "VELLUM_ENHANCED_COMMIT";
const ENHANCED_DEBUG_LOG_ENV: &str = "VELLUM_ENHANCED_DEBUG_LOG";
const LAUNCH_ID_ENV: &str = "VELLUM_LAUNCH_ID";

/// A build without the transport sidecar can expose Remote Control through
/// only one child. Keep that owner on Official Codex: Codex Desktop creates
/// its own conversations there, so assigning the relay to Enhanced makes the
/// phone list the shared on-disk thread and then fail to read or resume its
/// live Official writer.
const FALLBACK_REMOTE_CONTROL_PLANE: ExecutionPlane = ExecutionPlane::OfficialCodex;
const REMOTE_CONTROL_DISABLED_ENV: &str = "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED";

fn owns_fallback_remote_control(plane: ExecutionPlane) -> bool {
    plane == FALLBACK_REMOTE_CONTROL_PLANE
}

fn configure_fallback_remote_control(
    command: &mut Command,
    plane: ExecutionPlane,
    adopted_by_desktop: bool,
) {
    if !adopted_by_desktop || !owns_fallback_remote_control(plane) {
        command.env(REMOTE_CONTROL_DISABLED_ENV, "1");
    }
}

struct BridgeStatePaths {
    attestation: PathBuf,
    observations: PathBuf,
    _isolation: Option<tempfile::TempDir>,
}

fn bridge_is_started_by_codex_desktop() -> bool {
    let Some(parent_pid) = super::process_info::parent_pid(std::process::id()) else {
        return false;
    };
    super::process_info::executable_of(parent_pid)
        .as_deref()
        .is_some_and(super::desktop_manager::is_known_codex_desktop_executable)
}

fn should_isolate_bridge_state(adopted_by_desktop: bool, canonical_is_live: bool) -> bool {
    !adopted_by_desktop && canonical_is_live
}

fn bridge_state_paths(
    canonical: &Path,
    adopted_by_desktop: bool,
) -> Result<BridgeStatePaths, std::io::Error> {
    let canonical_is_live = BridgeAttestationV1::read(canonical)
        .ok()
        .is_some_and(|attestation| {
            attestation.is_live() && super::desktop_manager::adopted_by_codex_desktop(&attestation)
        });
    if should_isolate_bridge_state(adopted_by_desktop, canonical_is_live) {
        // CODEX_CLI_PATH is user-scoped, so helpers such as Computer Use can
        // invoke this bridge too. Their short-lived App Servers must not
        // overwrite the Desktop bridge's sole live status files.
        let isolation = tempfile::Builder::new()
            .prefix("vellum-transient-bridge-")
            .tempdir()?;
        return Ok(BridgeStatePaths {
            attestation: isolation.path().join("bridge-attestation.json"),
            observations: isolation.path().join("observations.json"),
            _isolation: Some(isolation),
        });
    }
    Ok(BridgeStatePaths {
        attestation: canonical.to_path_buf(),
        observations: canonical.with_file_name("observations.json"),
        _isolation: None,
    })
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub manifest: LaunchManifestV1,
    pub model_provider_map: TrustedModelProviderMap,
    pub child_args: Vec<String>,
}

impl BridgeConfig {
    pub fn load(manifest_path: &Path, child_args: Vec<String>) -> Result<Self, BridgeError> {
        let manifest = LaunchManifestV1::read(manifest_path)?;
        manifest.verify_on_disk()?;
        let model_provider_map = TrustedModelProviderMap::read(&manifest.model_provider_map_path)?;
        Ok(Self {
            manifest,
            model_provider_map,
            child_args,
        })
    }

    fn validate(&self) -> Result<(), BridgeError> {
        for (plane, identity) in [
            (ExecutionPlane::OfficialCodex, &self.manifest.official),
            (ExecutionPlane::EnhancedCodex, &self.manifest.enhanced),
        ] {
            if !identity.executable.is_absolute() || !identity.executable.is_file() {
                return Err(BridgeError::ExecutableUnavailable {
                    plane,
                    path: identity.executable.clone(),
                });
            }
            std::fs::create_dir_all(&identity.codex_home)?;
        }
        if let Some(parent) = self.manifest.binding_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

pub fn run_from_env() -> Result<(), BridgeError> {
    let child_args = env::args().skip(1).collect::<Vec<_>>();
    let manifest_path = LaunchManifestV1::default_path();
    if !child_args.iter().any(|arg| arg == "app-server") {
        return delegate_non_app_server(&manifest_path, child_args);
    }
    run(BridgeConfig::load(&manifest_path, child_args)?)
}

/// Codex Desktop also runs `CODEX_CLI_PATH` for one-shot commands. Those are
/// unmodified Official Codex behavior and must stay that way.
fn delegate_non_app_server(manifest_path: &Path, args: Vec<String>) -> Result<(), BridgeError> {
    let manifest = LaunchManifestV1::read(manifest_path)?;
    let status = Command::new(&manifest.official.executable)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(BridgeError::DelegatedCommandFailed(status.code()))
    }
}

pub fn run(config: BridgeConfig) -> Result<(), BridgeError> {
    config.validate()?;
    if config.manifest.relay.is_some() {
        return multiplex::run(config);
    }
    let manifest = config.manifest.clone();
    let adopted_by_desktop = bridge_is_started_by_codex_desktop();
    let state_paths = bridge_state_paths(&manifest.attestation_path, adopted_by_desktop)?;
    let mut attestation = AttestationWriter::new(
        state_paths.attestation.clone(),
        manifest.launch_id.clone(),
        manifest.model_provider_map_sha256.clone(),
        manifest.binding_db.clone(),
        ChildAttestation {
            binary_sha256: manifest.official.artifact_sha256.clone(),
            runtime_digest: manifest.official.runtime_digest.clone(),
            ..ChildAttestation::default()
        },
        ChildAttestation {
            binary_sha256: manifest.enhanced.artifact_sha256.clone(),
            runtime_digest: manifest.enhanced.runtime_digest.clone(),
            ..ChildAttestation::default()
        },
    );
    let _ = attestation.flush();

    let store = match ThreadRuntimeBindingStore::open(&manifest.binding_db) {
        Ok(store) => store,
        Err(error) => return Err(fail_attestation(&mut attestation, error.into())),
    };
    let bindings = match store.list() {
        Ok(bindings) => bindings,
        Err(error) => return Err(fail_attestation(&mut attestation, error.into())),
    };
    let providers = TrustedProviderSet::new(
        manifest.official_provider_ids.clone(),
        manifest.third_party_provider_ids.clone(),
    );
    let (events_tx, events_rx) = mpsc::channel();
    let mut official = match ChildProcess::spawn(
        ExecutionPlane::OfficialCodex,
        &manifest,
        &config.child_args,
        events_tx.clone(),
        adopted_by_desktop,
    ) {
        Ok(child) => child,
        Err(error) => return Err(fail_attestation(&mut attestation, error)),
    };
    attestation.set_child_pid(ExecutionPlane::OfficialCodex, official.pid());
    let mut enhanced = match ChildProcess::spawn(
        ExecutionPlane::EnhancedCodex,
        &manifest,
        &config.child_args,
        events_tx.clone(),
        adopted_by_desktop,
    ) {
        Ok(child) => child,
        Err(error) => {
            official.terminate();
            return Err(fail_attestation(&mut attestation, error));
        }
    };
    attestation.set_child_pid(ExecutionPlane::EnhancedCodex, enhanced.pid());
    let _ = attestation.flush();
    spawn_client_reader(events_tx);

    let stdout = std::io::stdout();
    let mut client_out = BufWriter::new(stdout.lock());
    let journal = QualificationJournal::new(
        manifest.qualification_journal_path.clone(),
        manifest.launch_id.clone(),
    );
    let mut state = BridgeState::new(
        store,
        providers,
        manifest.official.runtime_digest.clone(),
        manifest.enhanced.runtime_digest.clone(),
        config.model_provider_map,
        attestation,
        journal,
    );
    // Desktop builds currently ship without the optional relay sidecar, so
    // this is the production bridge path. Keep the same launch-scoped
    // observation file the relay path writes; otherwise the Enhanced Core tab
    // can report "no observations" while this bridge is actively serving.
    let observation_path = state_paths.observations.clone();
    let mut observations = RuntimeObservations::new(manifest.launch_id.clone());
    observations.seed_bindings(&bindings);
    let _ = observations.write(&observation_path);

    loop {
        let event = match events_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = observations.write(&observation_path);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let outcome = match event {
            BridgeEvent::Client(line) => match state.on_client_line(&line) {
                Ok(actions) => Ok(actions),
                Err(error) => Ok(vec![BridgeAction::ToClient(client_error(&line, &error))]),
            },
            BridgeEvent::Child(plane, line) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    observations.observe(plane.as_str(), &value);
                }
                state.on_child_line(plane, &line)
            }
            BridgeEvent::ClientClosed => break,
            BridgeEvent::ChildClosed(plane) => {
                state.mark_child_exited(plane, "app-server exited");
                let message = json!({
                    "method": "vellum/runtimeFailed",
                    "params": {"plane": plane.as_str(), "message": "app-server exited"}
                });
                write_json(&mut client_out, &message)?;
                continue;
            }
            BridgeEvent::ReadFailed(origin, message) => {
                state
                    .attestation
                    .mark_failed(format!("{origin}: {message}"));
                let _ = state.attestation.flush();
                return Err(BridgeError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{origin}: {message}"),
                )));
            }
        };
        match outcome {
            Ok(actions) => {
                for action in actions {
                    match action {
                        BridgeAction::ToChild(plane, value) => {
                            // Requests such as thread/read and thread/resume
                            // are already real observations before the child
                            // emits a notification. Recording both directions
                            // also keeps idle sessions visible.
                            observations.observe(plane.as_str(), &value);
                            match plane {
                                ExecutionPlane::OfficialCodex => official.send(&value)?,
                                ExecutionPlane::EnhancedCodex => enhanced.send(&value)?,
                            }
                        }
                        BridgeAction::ToClient(value) => write_json(&mut client_out, &value)?,
                        // No transport sidecar is packaged in this build, so
                        // one child must own the relay. Use Official because it
                        // owns Desktop's native conversations and their active
                        // writers. The shared CODEX_HOME makes those threads
                        // visible from Enhanced, but does not transfer their
                        // in-memory writer or subscriptions.
                        BridgeAction::ToRelay(value) => match FALLBACK_REMOTE_CONTROL_PLANE {
                            ExecutionPlane::OfficialCodex => official.send(&value)?,
                            ExecutionPlane::EnhancedCodex => enhanced.send(&value)?,
                        },
                    }
                }
            }
            Err(error) => {
                write_json(
                    &mut client_out,
                    &json!({
                        "method": "vellum/runtimeError",
                        "params": {"message": error.to_string()}
                    }),
                )?;
            }
        }
        let _ = observations.write(&observation_path);
    }
    official.terminate();
    enhanced.terminate();
    state.attestation.mark_stopped();
    let _ = state.attestation.flush();
    let _ = observations.write(&observation_path);
    Ok(())
}

fn fail_attestation(attestation: &mut AttestationWriter, error: BridgeError) -> BridgeError {
    attestation.mark_failed(error.to_string());
    let _ = attestation.flush();
    error
}

fn write_json(writer: &mut impl Write, value: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

struct ChildProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
}

impl ChildProcess {
    fn spawn(
        plane: ExecutionPlane,
        manifest: &LaunchManifestV1,
        args: &[String],
        events: mpsc::Sender<BridgeEvent>,
        adopted_by_desktop: bool,
    ) -> Result<Self, BridgeError> {
        let identity = match plane {
            ExecutionPlane::OfficialCodex => &manifest.official,
            ExecutionPlane::EnhancedCodex => &manifest.enhanced,
        };
        let mut command = Command::new(&identity.executable);
        command
            .args(args)
            // A child that inherited either of these would re-enter the bridge
            // and fork-bomb the machine.
            .env_remove("CODEX_CLI_PATH")
            .env_remove(super::launch_manifest::LAUNCH_MANIFEST_ENV)
            // Never let a value inherited from the bridge enable hooks in the
            // Official child. Enhanced receives both values explicitly below.
            .env_remove(FEATURE_PROFILE_ENV)
            .env_remove(ABLATION_PROFILE_ENV)
            .env_remove(ENHANCED_COMMIT_ENV)
            .env_remove(ENHANCED_DEBUG_LOG_ENV)
            .env("CODEX_HOME", &identity.codex_home)
            .env(PLANE_ENV, plane.as_str())
            .env(DIGEST_ENV, &identity.runtime_digest)
            .env(LAUNCH_ID_ENV, &manifest.launch_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // Both cores share one installation id, and the Remote Control service
        // accepts exactly one online app server for it. Without the optional
        // multi-client relay, allowing both children to connect creates a
        // startup race: whichever child wins can only serve threads owned by
        // that plane, while the other retries forever with HTTP 409. Keep the
        // documented fallback owner deterministic.
        configure_fallback_remote_control(&mut command, plane, adopted_by_desktop);
        if plane == ExecutionPlane::EnhancedCodex {
            let features: EnhancedRuntimeFeatures = manifest.feature_profile.clone().into();
            command
                .env(ENHANCED_COMMIT_ENV, &manifest.enhanced_commit)
                .env(
                    ENHANCED_DEBUG_LOG_ENV,
                    identity
                        .codex_home
                        .join("log")
                        .join("enhanced-events.jsonl"),
                )
                .env(
                    FEATURE_PROFILE_ENV,
                    serde_json::to_string(&manifest.feature_profile).unwrap_or_default(),
                );
            if let Some(profile) = ablation_profile_label(features) {
                command.env(ABLATION_PROFILE_ENV, profile);
            }
        }
        let mut child = command.spawn().map_err(|source| BridgeError::Spawn {
            plane,
            path: identity.executable.clone(),
            source,
        })?;
        let stdin = child.stdin.take().ok_or(BridgeError::MissingPipe(plane))?;
        let stdout = child.stdout.take().ok_or(BridgeError::MissingPipe(plane))?;
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if events.send(BridgeEvent::Child(plane, line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = events.send(BridgeEvent::ReadFailed(
                            plane.as_str().into(),
                            error.to_string(),
                        ));
                        return;
                    }
                }
            }
            let _ = events.send(BridgeEvent::ChildClosed(plane));
        });
        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, value: &Value) -> Result<(), BridgeError> {
        write_json(&mut self.stdin, value)
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ablation_profile_label(features: EnhancedRuntimeFeatures) -> Option<&'static str> {
    [
        AblationProfile::E0,
        AblationProfile::E1,
        AblationProfile::E2,
        AblationProfile::E3,
        AblationProfile::E4,
        AblationProfile::E5,
    ]
    .into_iter()
    .find(|profile| profile.features() == features)
    .map(AblationProfile::as_str)
}

fn spawn_client_reader(events: mpsc::Sender<BridgeEvent>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if events.send(BridgeEvent::Client(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ =
                        events.send(BridgeEvent::ReadFailed("desktop".into(), error.to_string()));
                    return;
                }
            }
        }
        let _ = events.send(BridgeEvent::ClientClosed);
    });
}

enum BridgeEvent {
    Client(String),
    Child(ExecutionPlane, String),
    ClientClosed,
    ChildClosed(ExecutionPlane),
    ReadFailed(String, String),
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum BridgeAction {
    ToChild(ExecutionPlane, Value),
    ToRelay(Value),
    ToClient(Value),
}

#[derive(Debug, Clone)]
struct PendingBinding {
    plane: ExecutionPlane,
    provider_id: String,
    model_id: String,
    expected_thread_id: Option<String>,
}

struct BridgeState {
    store: ThreadRuntimeBindingStore,
    providers: TrustedProviderSet,
    official_digest: String,
    enhanced_digest: String,
    model_provider_map: TrustedModelProviderMap,
    pending_bindings: HashMap<String, PendingBinding>,
    /// Threads whose turn the bridge has seen begin and not seen end, with the
    /// plane serving them so a child that dies takes only its own turns down.
    open_turns: HashMap<String, ExecutionPlane>,
    /// Turn requests still awaiting a response, and how each one ends.
    turn_requests: HashMap<String, TurnRequest>,
    shadow_requests: HashSet<String>,
    initialize_requests: HashSet<String>,
    server_requests: HashMap<String, (ExecutionPlane, Value)>,
    next_server_request: u64,
    official_exited: bool,
    enhanced_exited: bool,
    attestation: AttestationWriter,
    journal: QualificationJournal,
}

/// A turn request the client sent and the child has not answered.
struct TurnRequest {
    thread_id: String,
    /// `turn/create` carries the whole turn and is finished when it answers.
    /// `turn/start` only acknowledges; that turn ends on `turn/completed`.
    closes_on_response: bool,
}

impl BridgeState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: ThreadRuntimeBindingStore,
        providers: TrustedProviderSet,
        official_digest: String,
        enhanced_digest: String,
        model_provider_map: TrustedModelProviderMap,
        attestation: AttestationWriter,
        journal: QualificationJournal,
    ) -> Self {
        Self {
            store,
            providers,
            official_digest,
            enhanced_digest,
            model_provider_map,
            pending_bindings: HashMap::new(),
            open_turns: HashMap::new(),
            turn_requests: HashMap::new(),
            shadow_requests: HashSet::new(),
            initialize_requests: HashSet::new(),
            server_requests: HashMap::new(),
            next_server_request: 1,
            official_exited: false,
            enhanced_exited: false,
            attestation,
            journal,
        }
    }

    fn mark_child_exited(&mut self, plane: ExecutionPlane, reason: &str) {
        // Whatever that child was in the middle of is not in flight any more.
        // Leaving the count up would refuse every restart from here on.
        self.open_turns.retain(|_, serving| *serving != plane);
        self.publish_open_turns();

        match plane {
            ExecutionPlane::OfficialCodex => self.official_exited = true,
            ExecutionPlane::EnhancedCodex => self.enhanced_exited = true,
        }
        self.attestation.mark_exited(plane);
        self.attestation
            .set_failure_reason(format!("{}: {reason}", plane.as_str()));
        let _ = self.attestation.flush();
    }

    /// A dead child never gets traffic silently rerouted to the other plane —
    /// that reroute is precisely the false success this gate exists to stop.
    fn ensure_plane_available(&self, plane: ExecutionPlane) -> Result<(), BridgeError> {
        let exited = match plane {
            ExecutionPlane::OfficialCodex => self.official_exited,
            ExecutionPlane::EnhancedCodex => self.enhanced_exited,
        };
        if exited {
            return Err(BridgeError::RuntimeUnavailable { plane });
        }
        Ok(())
    }

    /// Codex Desktop asked for a turn.
    ///
    /// Vellum decides whether a restart is safe by reading the attestation, and
    /// until this existed the answer came from a counter that only sees Proxy
    /// traffic. An Official-plane turn never touches that counter, so "idle"
    /// was true of Vellum and false of Codex, and a restart on that answer
    /// destroyed a thread that had not been written to disk yet.
    fn open_turn(
        &mut self,
        method: &str,
        params: &Value,
        plane: ExecutionPlane,
        id: Option<&Value>,
    ) {
        let closes_on_response = match method {
            "turn/create" => true,
            "turn/start" => false,
            _ => return,
        };
        let Some(thread_id) = find_thread_id(params).map(str::to_string) else {
            return;
        };
        if let Some(id) = id {
            self.turn_requests.insert(
                id_key(id),
                TurnRequest {
                    thread_id: thread_id.clone(),
                    closes_on_response,
                },
            );
        }
        self.open_turns.insert(thread_id, plane);
        self.publish_open_turns();
    }

    fn close_turn(&mut self, thread_id: &str) {
        if self.open_turns.remove(thread_id).is_some() {
            self.publish_open_turns();
        }
    }

    fn publish_open_turns(&mut self) {
        let open = self.open_turns.len() as u32;
        self.attestation.set_open_turns(open);
    }

    fn on_client_line(&mut self, line: &str) -> Result<Vec<BridgeAction>, BridgeError> {
        let mut value: Value = serde_json::from_str(line)?;
        if value.get("method").is_none() {
            return self.route_client_response(value);
        }
        let method = value["method"].as_str().unwrap_or_default();
        if super::contracts::is_remote_control(method) {
            return Ok(vec![BridgeAction::ToRelay(value)]);
        }
        if method == "initialized" {
            return Ok(vec![
                BridgeAction::ToChild(ExecutionPlane::OfficialCodex, value.clone()),
                BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, value),
            ]);
        }
        if method == "initialize" {
            let Some(id) = value.get("id").cloned() else {
                return Err(BridgeError::Protocol("initialize request has no id".into()));
            };
            self.initialize_requests.insert(id_key(&id));
            let translated = format!("vellum:shadow:initialize:{}", self.next_server_request);
            self.next_server_request += 1;
            self.shadow_requests
                .insert(id_key(&Value::String(translated.clone())));
            let mut shadow = value.clone();
            shadow["id"] = Value::String(translated);
            return Ok(vec![
                BridgeAction::ToChild(ExecutionPlane::OfficialCodex, value),
                BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, shadow),
            ]);
        }
        let id = value.get("id").cloned();
        let params = value.get("params").unwrap_or(&Value::Null);
        let (plane, pending, child_provider_id) = self.route_method(method, params)?;
        self.ensure_plane_available(plane)?;
        self.open_turn(method, params, plane, id.as_ref());
        if let Some(child_provider_id) = child_provider_id {
            value["params"]["modelProvider"] = Value::String(child_provider_id);
        }
        if let (Some(id), Some(pending)) = (id.as_ref(), pending) {
            self.pending_bindings.insert(id_key(id), pending);
        }
        Ok(vec![BridgeAction::ToChild(plane, value)])
    }

    fn route_client_response(
        &mut self,
        mut value: Value,
    ) -> Result<Vec<BridgeAction>, BridgeError> {
        let Some(id) = value.get("id") else {
            return Err(BridgeError::Protocol("response without id".into()));
        };
        let key = id_key(id);
        let Some((plane, original_id)) = self.server_requests.remove(&key) else {
            return Err(BridgeError::Protocol(format!(
                "unknown server request response id {key}"
            )));
        };
        self.ensure_plane_available(plane)?;
        value["id"] = original_id;
        Ok(vec![BridgeAction::ToChild(plane, value)])
    }

    fn route_method(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<(ExecutionPlane, Option<PendingBinding>, Option<String>), BridgeError> {
        // Every frontend uses the same immutable task binding. Relay management
        // is intercepted before this execution-plane router.
        match method {
            "thread/start" => {
                let (pending, child_provider_id) =
                    self.pending_from_provider(params, None, false)?;
                Ok((pending.plane, Some(pending), Some(child_provider_id)))
            }
            "thread/resume" => {
                let thread_id = required_string(params, "threadId")?;
                if let Some(binding) = self.store.get(thread_id)? {
                    let child_provider_id =
                        self.ensure_provider_does_not_switch(params, &binding)?;
                    return Ok((binding.plane, None, child_provider_id));
                }
                let (pending, child_provider_id) =
                    self.pending_from_provider(params, Some(thread_id.into()), true)?;
                Ok((pending.plane, Some(pending), Some(child_provider_id)))
            }
            "thread/fork" => {
                let thread_id = required_string(params, "threadId")?;
                let binding = self.require_binding(thread_id)?;
                let child_provider_id = self.ensure_provider_does_not_switch(params, &binding)?;
                Ok((
                    binding.plane,
                    Some(PendingBinding {
                        plane: binding.plane,
                        provider_id: binding.provider_id,
                        model_id: params
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or(&binding.model_id)
                            .to_string(),
                        expected_thread_id: None,
                    }),
                    child_provider_id,
                ))
            }
            "initialize" | "model/list" | "account/read" | "config/read" => {
                Ok((ExecutionPlane::OfficialCodex, None, None))
            }
            _ => {
                if let Some(thread_id) = find_thread_id(params) {
                    return Ok((self.require_binding(thread_id)?.plane, None, None));
                }
                if super::contracts::requires_thread(method) {
                    return Err(BridgeError::Protocol("thread owner is required".into()));
                }
                Ok((ExecutionPlane::OfficialCodex, None, None))
            }
        }
    }

    fn pending_from_provider(
        &self,
        params: &Value,
        expected_thread_id: Option<String>,
        allow_legacy_official_discovery: bool,
    ) -> Result<(PendingBinding, String), BridgeError> {
        let model_id = params
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (provider_id, child_provider_id) = if let Some(route) =
            self.model_provider_map.resolve(model_id)
        {
            (route.provider_id.clone(), route.child_provider_id.clone())
        } else if let Some(provider_id) = params
            .get("modelProvider")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            let child_provider = if provider_id == "openai" || provider_id == "openai-official" {
                crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME.to_string()
            } else {
                provider_id.to_string()
            };
            (provider_id.to_string(), child_provider)
        } else if allow_legacy_official_discovery && model_id.trim().is_empty() {
            // Desktop versions predating the bridge resume existing
            // Official threads without either routing hint. Probe only
            // that legacy resume path against Official. A binding is not
            // persisted unless Official successfully returns the exact
            // requested thread id.
            (
                "openai-official".to_string(),
                crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME.to_string(),
            )
        } else {
            return Err(BridgeError::MissingRoutingAuthority);
        };
        let plane = match self.providers.classify(&provider_id)? {
            super::ProviderClass::Official => ExecutionPlane::OfficialCodex,
            super::ProviderClass::TrustedThirdParty => ExecutionPlane::EnhancedCodex,
        };
        Ok((
            PendingBinding {
                plane,
                provider_id,
                model_id: model_id.into(),
                expected_thread_id,
            },
            child_provider_id,
        ))
    }

    fn require_binding(&self, thread_id: &str) -> Result<ThreadRuntimeBinding, BridgeError> {
        self.store
            .get(thread_id)?
            .ok_or_else(|| BridgeError::UnboundThread(thread_id.into()))
    }

    fn ensure_provider_does_not_switch(
        &self,
        params: &Value,
        binding: &ThreadRuntimeBinding,
    ) -> Result<Option<String>, BridgeError> {
        let model_id = params
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&binding.model_id);
        if let Some(route) = self.model_provider_map.resolve(model_id) {
            let is_same_official = (route.provider_id == "openai"
                || route.provider_id == "openai-official")
                && (binding.provider_id == "openai" || binding.provider_id == "openai-official");
            if route.provider_id != binding.provider_id && !is_same_official {
                return Err(BridgeError::ProviderSwitchForbidden {
                    thread_id: binding.thread_id.clone(),
                    bound: binding.provider_id.clone(),
                    requested: route.provider_id.clone(),
                });
            }
            return Ok(Some(route.child_provider_id.clone()));
        }
        if let Some(provider) = params.get("modelProvider").and_then(Value::as_str) {
            let is_same_official = (provider == "openai" || provider == "openai-official")
                && (binding.provider_id == "openai" || binding.provider_id == "openai-official");
            if provider != binding.provider_id && !is_same_official {
                return Err(BridgeError::ProviderSwitchForbidden {
                    thread_id: binding.thread_id.clone(),
                    bound: binding.provider_id.clone(),
                    requested: provider.into(),
                });
            }
        }
        Ok(None)
    }

    fn on_child_line(
        &mut self,
        plane: ExecutionPlane,
        line: &str,
    ) -> Result<Vec<BridgeAction>, BridgeError> {
        let mut value: Value = serde_json::from_str(line)?;
        if value.get("method").is_some() && value.get("id").is_some() {
            let original_id = value["id"].clone();
            let translated = format!("vellum:{}:{}", plane.as_str(), self.next_server_request);
            self.next_server_request += 1;
            self.server_requests.insert(
                id_key(&Value::String(translated.clone())),
                (plane, original_id),
            );
            value["id"] = Value::String(translated);
            return Ok(vec![BridgeAction::ToClient(value)]);
        }
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            if method.starts_with("vellum/") {
                return Ok(self.absorb_vellum_notification(plane, &method, &value));
            }
            // A native collaboration spawn creates its child inside Codex, so
            // Desktop never sends the bridge a `thread/start` request for that
            // child. Learn the child binding from Codex's own typed lifecycle
            // item before applying the normal notification ownership gate.
            // Without this, the spawn card reaches Desktop but every child
            // notification and later `thread/read` is rejected as unbound,
            // leaving the official sub-agent conversation UI empty.
            self.bind_spawned_agent_threads(plane, &value)?;
            if method == "turn/completed" {
                if let Some(thread_id) = value.get("params").and_then(find_thread_id) {
                    let thread_id = thread_id.to_string();
                    self.close_turn(&thread_id);
                }
            }
            if let Some(thread_id) = notification_owner_thread_id(&method, &value) {
                if self.require_binding(thread_id)?.plane != plane {
                    return Ok(Vec::new());
                }
            } else if plane == ExecutionPlane::EnhancedCodex {
                // The Enhanced child shares the client connection with Official.
                // A notification it cannot tie to one of its own threads has no
                // addressee, so it stops here rather than reaching Desktop.
                self.journal
                    .record_rejected(&method, "enhanced notification has no bound thread");
                return Ok(Vec::new());
            }
            return Ok(vec![BridgeAction::ToClient(value)]);
        }
        if let Some(id) = value.get("id") {
            let key = id_key(id);
            if self.shadow_requests.remove(&key) {
                if let Some(error) = value.get("error") {
                    self.attestation
                        .set_failure_reason(format!("enhanced initialize failed: {error}"));
                    let _ = self.attestation.flush();
                    return Ok(vec![BridgeAction::ToClient(json!({
                        "method": "vellum/runtimeFailed",
                        "params": {
                            "plane": plane.as_str(),
                            "message": error.to_string()
                        }
                    }))]);
                }
                self.attestation.mark_initialized(plane);
                let _ = self.attestation.flush();
                return Ok(Vec::new());
            }
            if self.initialize_requests.remove(&key) && value.get("error").is_none() {
                self.attestation.mark_initialized(plane);
                let _ = self.attestation.flush();
            }
            if let Some(turn) = self.turn_requests.remove(&key) {
                // A refused `turn/start` never became a turn, so it closes on
                // the error even though a successful one waits for the
                // `turn/completed` notification.
                if turn.closes_on_response || value.get("error").is_some() {
                    self.close_turn(&turn.thread_id);
                }
            }
            if let Some(pending) = self.pending_bindings.remove(&key) {
                if pending.plane != plane {
                    return Err(BridgeError::Protocol(
                        "response came from wrong runtime".into(),
                    ));
                }
                if value.get("error").is_none() {
                    let returned = response_thread_id(&value)?;
                    if let Some(expected) = pending.expected_thread_id.as_deref() {
                        if expected != returned {
                            return Err(BridgeError::Protocol(format!(
                                "resume returned thread {returned}, expected {expected}"
                            )));
                        }
                    }
                    let digest = match plane {
                        ExecutionPlane::OfficialCodex => &self.official_digest,
                        ExecutionPlane::EnhancedCodex => &self.enhanced_digest,
                    };
                    let binding = ThreadRuntimeBinding::new(
                        returned,
                        plane,
                        digest,
                        pending.provider_id,
                        pending.model_id,
                        returned,
                        Utc::now().timestamp(),
                    );
                    if let Err(error) = self.store.insert_immutable(&binding) {
                        value = json_rpc_error(value["id"].clone(), -32071, error.to_string());
                    }
                }
            }
        }
        Ok(vec![BridgeAction::ToClient(value)])
    }

    fn bind_spawned_agent_threads(
        &self,
        plane: ExecutionPlane,
        notification: &Value,
    ) -> Result<(), BridgeError> {
        let Some(params) = notification.get("params") else {
            return Ok(());
        };

        // A native child announces its complete Thread object before the
        // parent's spawnAgent item is necessarily completed.  Waiting for the
        // later item leaves a real race: Desktop can observe or address the
        // child while it is still unbound, and the bridge used to discard that
        // first lifecycle notification.  `parentThreadId` is typed App Server
        // authority, so bind from it immediately and verify the child did not
        // silently select another provider table.
        if notification.get("method").and_then(Value::as_str) == Some("thread/started") {
            let Some(thread) = params.get("thread") else {
                return Err(BridgeError::Protocol(
                    "thread/started notification has no thread".into(),
                ));
            };
            let Some(parent_thread_id) = thread
                .get("parentThreadId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            else {
                return Ok(());
            };
            let child_thread_id = required_string(thread, "id")?;
            let actual_provider = required_string(thread, "modelProvider")?;
            let parent = self.require_binding(parent_thread_id)?;
            if parent.plane != plane {
                return Err(BridgeError::Protocol(format!(
                    "child thread {child_thread_id} was announced by the wrong runtime"
                )));
            }
            let expected_provider = self.child_provider_for_binding(&parent)?;
            if actual_provider != expected_provider {
                return Err(BridgeError::ProviderSwitchForbidden {
                    thread_id: child_thread_id.into(),
                    bound: expected_provider,
                    requested: actual_provider.into(),
                });
            }
            return self.inherit_child_binding(&parent, child_thread_id, &parent.model_id);
        }

        let Some(parent_thread_id) = find_thread_id(params) else {
            return Ok(());
        };
        let Some(item) = params.get("item") else {
            return Ok(());
        };

        let child_thread_ids = match item.get("type").and_then(Value::as_str) {
            Some("collabAgentToolCall")
                if item.get("tool").and_then(Value::as_str) == Some("spawnAgent") =>
            {
                let sender_thread_id = item
                    .get("senderThreadId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        BridgeError::Protocol(
                            "spawnAgent lifecycle item has no senderThreadId".into(),
                        )
                    })?;
                if sender_thread_id != parent_thread_id {
                    return Err(BridgeError::Protocol(format!(
                        "spawnAgent sender thread {sender_thread_id} does not match notification thread {parent_thread_id}"
                    )));
                }
                item.get("receiverThreadIds")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        BridgeError::Protocol(
                            "spawnAgent lifecycle item has no receiverThreadIds".into(),
                        )
                    })?
                    .iter()
                    .map(|thread_id| {
                        thread_id
                            .as_str()
                            .filter(|id| !id.trim().is_empty())
                            .ok_or_else(|| {
                                BridgeError::Protocol(
                                "spawnAgent receiverThreadIds contains a non-string or empty id"
                                    .into(),
                            )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            // Retain the pinned legacy lifecycle shape as a compatibility
            // authority. Current Desktop schemas accept both item variants.
            Some("subAgentActivity") => vec![item
                .get("agentThreadId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    BridgeError::Protocol(
                        "subAgentActivity lifecycle item has no agentThreadId".into(),
                    )
                })?],
            _ => return Ok(()),
        };

        if child_thread_ids.is_empty() {
            return Ok(());
        }
        let parent = self.require_binding(parent_thread_id)?;
        if parent.plane != plane {
            return Err(BridgeError::Protocol(format!(
                "spawn lifecycle for thread {parent_thread_id} came from the wrong runtime"
            )));
        }
        let requested_model = item
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or(&parent.model_id);
        if let Some(route) = self.model_provider_map.resolve(requested_model) {
            let same_official = (route.provider_id == "openai"
                || route.provider_id == "openai-official")
                && (parent.provider_id == "openai" || parent.provider_id == "openai-official");
            if route.provider_id != parent.provider_id && !same_official {
                return Err(BridgeError::ProviderSwitchForbidden {
                    thread_id: parent_thread_id.into(),
                    bound: parent.provider_id.clone(),
                    requested: route.provider_id.clone(),
                });
            }
        } else if requested_model != parent.model_id {
            return Err(BridgeError::MissingRoutingAuthority);
        }
        for child_thread_id in child_thread_ids {
            // `thread/started` may already have committed this binding before
            // the lifecycle item arrives. The Thread schema carries the
            // provider but not the requested model, so retain the parent's
            // route authority here; the proxy separately validates any child
            // model override against that exact parent route.
            self.inherit_child_binding(&parent, child_thread_id, &parent.model_id)?;
        }
        Ok(())
    }

    fn child_provider_for_binding(
        &self,
        binding: &ThreadRuntimeBinding,
    ) -> Result<String, BridgeError> {
        if let Some(route) = self.model_provider_map.resolve(&binding.model_id) {
            return Ok(route.child_provider_id.clone());
        }
        Ok(match binding.plane {
            ExecutionPlane::OfficialCodex => {
                crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME.to_string()
            }
            ExecutionPlane::EnhancedCodex => crate::codex::VELLUM_PROVIDER_NAME.to_string(),
        })
    }

    fn inherit_child_binding(
        &self,
        parent: &ThreadRuntimeBinding,
        child_thread_id: &str,
        model_id: &str,
    ) -> Result<(), BridgeError> {
        let child = ThreadRuntimeBinding::new(
            child_thread_id,
            parent.plane,
            parent.runtime_digest.clone(),
            parent.provider_id.clone(),
            model_id,
            child_thread_id,
            Utc::now().timestamp(),
        );
        self.store.insert_immutable(&child)?;
        Ok(())
    }

    /// `vellum/*` notifications are the Enhanced fork talking to Vellum, not to
    /// Codex Desktop. They are recorded here and go no further.
    fn absorb_vellum_notification(
        &mut self,
        plane: ExecutionPlane,
        method: &str,
        value: &Value,
    ) -> Vec<BridgeAction> {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        if plane != ExecutionPlane::EnhancedCodex {
            self.journal
                .record_rejected(method, "only the Enhanced child may report enhanced state");
            return Vec::new();
        }
        match method {
            ENHANCED_IDENTITY_NOTIFICATION => match parse_enhanced_identity(&params) {
                Ok(identity) => {
                    self.journal.record_identity(&identity);
                    self.attestation.set_enhanced_identity(identity);
                    let _ = self.attestation.flush();
                }
                Err(reason) => self.journal.record_rejected(method, reason),
            },
            ENHANCED_EVENT_NOTIFICATION => match parse_enhanced_event(&params) {
                Ok(event) => {
                    if event.name == EnhancedEventKind::SessionFeaturesApplied.name() {
                        self.attestation.record_session_features_applied();
                    }
                    self.journal.record_event(event);
                }
                Err(reason) => self.journal.record_rejected(method, reason),
            },
            _ => self
                .journal
                .record_rejected(method, "not part of the enhanced notification contract"),
        }
        Vec::new()
    }
}

fn find_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .or_else(|| params.get("conversationId"))
        .and_then(Value::as_str)
}

fn notification_owner_thread_id<'a>(method: &str, value: &'a Value) -> Option<&'a str> {
    let params = value.get("params")?;
    find_thread_id(params).or_else(|| {
        (method == "thread/started"
            && params
                .pointer("/thread/parentThreadId")
                .and_then(Value::as_str)
                .is_some())
        .then(|| params.pointer("/thread/id").and_then(Value::as_str))
        .flatten()
    })
}

fn response_thread_id(value: &Value) -> Result<&str, BridgeError> {
    value
        .pointer("/result/thread/id")
        .or_else(|| value.pointer("/result/threadId"))
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::Protocol("thread response did not contain a thread id".into()))
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, BridgeError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| BridgeError::Protocol(format!("missing required {key}")))
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

fn client_error(line: &str, error: &BridgeError) -> Value {
    let id = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(Value::Null);
    json_rpc_error(id, -32070, error.to_string())
}

fn id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".into())
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("{plane:?} Codex executable is unavailable: {}", path.display())]
    ExecutableUnavailable {
        plane: ExecutionPlane,
        path: PathBuf,
    },
    #[error("cannot start {plane:?} Codex from {}: {source}", path.display())]
    Spawn {
        plane: ExecutionPlane,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0:?} Codex did not expose stdio pipes")]
    MissingPipe(ExecutionPlane),
    #[error("delegated Codex command failed with exit code {0:?}")]
    DelegatedCommandFailed(Option<i32>),
    #[error("app-server protocol error: {0}")]
    Protocol(String),
    #[error(
        "app-server protocol error: thread/start requires a mapped model or trusted modelProvider"
    )]
    MissingRoutingAuthority,
    #[error("thread {0} has no immutable runtime binding")]
    UnboundThread(String),
    #[error("{plane:?} Codex is not running; this request fails closed rather than falling back to the other runtime")]
    RuntimeUnavailable { plane: ExecutionPlane },
    #[error("thread {thread_id} is bound to provider {bound}; switching to {requested} requires a new thread")]
    ProviderSwitchForbidden {
        thread_id: String,
        bound: String,
        requested: String,
    },
    #[error(transparent)]
    LaunchManifest(#[from] LaunchManifestError),
    #[error(transparent)]
    Router(#[from] super::RouterError),
    #[error(transparent)]
    Binding(#[from] super::BindingStoreError),
    #[error(transparent)]
    ModelProviderMap(#[from] super::model_provider_map::ModelProviderMapError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
#[path = "app_server_bridge_tests.rs"]
mod tests;
