//! Durable handoff orchestration. Only the existing daemon executes turns.
//! Persist intent before each non-idempotent RPC. Replay requires an explicit
//! start and pinned-daemon evidence that the original mutation never materialized.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mutation_lock::HostMutationGuard;
use crate::state::AgentPaths;

// Pinned rust-v0.147.0-alpha.6.6: includeTurns on an unmaterialized thread
// returns JSON-RPC -32600 with this message. Do not match transport failures,
// "no rollout", or arbitrary errors merely mentioning a thread.
const PINNED_0147_NOT_MATERIALIZED: &str = "not materialized";
fn not_materialized(error: &str) -> bool {
    error
        .strip_prefix("HandoffDaemonRpcError: thread/read: ")
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .is_some_and(|body| {
            body["code"].as_i64() == Some(-32600)
                && body["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(PINNED_0147_NOT_MATERIALIZED))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub operation_id: String,
    pub repo_id: String,
    pub snapshot_commit: String,
    pub source_thread_id: String,
    pub brief: String,
    pub model: String,
    pub model_provider: String,
    pub effort: String,
    /// Prepare the private bare repo, return its path, then push the snapshot
    /// ref over managed SSH and repeat this same request with false.
    #[serde(default)]
    pub prepare_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResultRequest {
    pub handoff_id: String,
    #[serde(default)]
    pub capture: bool,
    #[serde(default)]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub path: String,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    pub result_ref: String,
    pub commit: String,
    pub snapshot_commit: String,
    pub partial: bool,
    pub diffstat: Vec<FileStat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffStatus {
    pub handoff_id: String,
    pub source_thread_id: String,
    pub state: String,
    pub phase: Phase,
    pub repo_path: String,
    pub worktree: String,
    pub snapshot_ref: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub turn_status: Option<String>,
    pub plan: Vec<String>,
    pub recent_output: String,
    pub summary: String,
    pub capture: Option<Capture>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Prepared,
    WorktreeReady,
    ThreadStarting,
    ThreadAdopted,
    ThreadReady,
    TurnStarting,
    Running,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    request: StartRequest,
    fingerprint: String,
    phase: Phase,
    thread_id: Option<String>,
    turn_id: Option<String>,
    captures: BTreeMap<String, Capture>,
    #[serde(default)]
    capture_intents: BTreeMap<String, bool>,
}

#[derive(Debug)]
pub(crate) struct Layout {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub snapshot_ref: String,
    pub object_format: &'static str,
}
impl Layout {
    fn new(paths: &AgentPaths, request: &StartRequest) -> Self {
        let root = paths.root.join("handoffs");
        Self {
            repo: root.join("repos").join(format!("{}.git", request.repo_id)),
            worktree: root.join("work").join(&request.operation_id),
            snapshot_ref: format!("refs/vellum-handoff/{}", request.operation_id),
            object_format: if request.snapshot_commit.len() == 64 {
                "sha256"
            } else {
                "sha1"
            },
            root,
        }
    }
}

pub(crate) trait Client {
    /// Read-only; refuses incomplete runtime, unavailable/unmanaged daemon.
    fn preflight(&self) -> Result<(), String>;
    fn prepare_repo(&self, layout: &Layout) -> Result<(), String>;
    fn prepare_worktree(&self, layout: &Layout, snapshot: &str) -> Result<(), String>;
    /// Returns JSON-RPC result; transport/RPC errors are errors, never defaults.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
    fn capture(
        &self,
        layout: &Layout,
        snapshot: &str,
        result_ref: &str,
        partial: bool,
    ) -> Result<Capture, String>;
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
pub(crate) fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn validate(request: &StartRequest) -> Result<(), String> {
    if !valid_id(&request.operation_id)
        || !valid_id(&request.repo_id)
        || !valid_commit(&request.snapshot_commit)
        || request.source_thread_id.is_empty()
        || request.source_thread_id.len() > 256
        || request.brief.trim().is_empty()
        || request.brief.len() > 128 * 1024
        || [&request.model, &request.model_provider]
            .iter()
            .any(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        || !matches!(
            request.effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        )
    {
        return Err("HandoffRequestInvalid".into());
    }
    Ok(())
}

pub(crate) fn private_dir(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() {
            return Err(format!("HandoffPathUnsafe: {}", path.display()));
        }
    } else {
        fs::create_dir(path)
            .map_err(|e| format!("create handoff directory {}: {e}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn record_path(paths: &AgentPaths, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err("HandoffRequestInvalid: invalid handoff id".into());
    }
    Ok(paths
        .root
        .join("handoffs/records")
        .join(format!("{id}.json")))
}
fn load(paths: &AgentPaths, id: &str) -> Result<Record, String> {
    let path = record_path(paths, id)?;
    for directory in [
        paths.root.join("handoffs"),
        paths.root.join("handoffs/records"),
    ] {
        if !fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err("HandoffPathUnsafe: missing or linked records directory".into());
        }
    }
    crate::update::reject_symlink(&path)?;
    let raw = fs::read(path).map_err(|e| format!("HandoffRecordUnavailable: {e}"))?;
    let record: Record =
        serde_json::from_slice(&raw).map_err(|e| format!("HandoffRecordInvalid: {e}"))?;
    validate(&record.request)?;
    if record.request.operation_id != id {
        return Err("HandoffRecordIdentityMismatch".into());
    }
    Ok(record)
}
fn save(paths: &AgentPaths, record: &Record) -> Result<(), String> {
    // The mutation lock creates agent/root parents; never adopt a linked handoff root.
    let root = paths.root.join("handoffs");
    private_dir(&root)?;
    private_dir(&root.join("records"))?;
    let path = record_path(paths, &record.request.operation_id)?;
    crate::operations::atomic_write(
        &path,
        &serde_json::to_vec(record).map_err(|e| e.to_string())?,
    )
}
fn status(record: &Record, layout: &Layout) -> HandoffStatus {
    HandoffStatus {
        handoff_id: record.request.operation_id.clone(),
        source_thread_id: record.request.source_thread_id.clone(),
        state: match record.phase {
            Phase::Prepared | Phase::WorktreeReady | Phase::ThreadReady => "prepared",
            Phase::ThreadStarting | Phase::ThreadAdopted | Phase::TurnStarting => "indeterminate",
            Phase::Running => "running",
        }
        .into(),
        phase: record.phase,
        repo_path: layout.repo.to_string_lossy().into_owned(),
        worktree: layout.worktree.to_string_lossy().into_owned(),
        snapshot_ref: layout.snapshot_ref.clone(),
        thread_id: record.thread_id.clone(),
        turn_id: record.turn_id.clone(),
        turn_status: None,
        plan: Vec::new(),
        recent_output: String::new(),
        summary: String::new(),
        capture: None,
    }
}

pub fn start(
    paths: &AgentPaths,
    home: &Path,
    request: StartRequest,
) -> Result<HandoffStatus, String> {
    start_with(paths, request, &crate::handoff_git::NativeClient::new(home))
}

/// Fixed, read-only tool discovery for Desktop preflight. Only Git's version
/// probe is executed; task commands and renderer-supplied names are forbidden.
pub fn preflight(home: &Path) -> Result<Value, String> {
    let client = crate::handoff_git::NativeClient::new(home);
    client.preflight()?;
    let output = std::process::Command::new("sh").args(["-c",
        "for tool in git python python3 node pnpm cargo dotnet; do if command -v \"$tool\" >/dev/null 2>&1; then printf '%s\\n' \"$tool\"; fi; done"
    ]).output().map_err(|e| format!("HandoffToolProbeFailed: {e}"))?;
    if !output.status.success() {
        return Err("HandoffToolProbeFailed".into());
    }
    let tools: Vec<_> = std::str::from_utf8(&output.stdout)
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::to_owned)
        .collect();
    let git_ready = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && git_object_format_supported(&String::from_utf8_lossy(&output.stdout))
        });
    let mut report = preflight_metadata(home, tools)?;
    report["gitObjectFormatSupported"] = json!(git_ready);
    Ok(report)
}

fn git_object_format_supported(version: &str) -> bool {
    let mut numbers = version
        .strip_prefix("git version ")
        .unwrap_or_default()
        .split('.');
    matches!((numbers.next().and_then(|v| v.parse::<u32>().ok()), numbers.next().and_then(|v| v.parse::<u32>().ok())),
        (Some(major), Some(minor)) if major > 2 || major == 2 && minor >= 29)
}

fn preflight_metadata(home: &Path, tools: Vec<String>) -> Result<Value, String> {
    let config: toml::Value = fs::read_to_string(home.join("config.toml"))
        .map_err(|e| format!("HandoffConfigurationUnavailable: {e}"))?
        .parse()
        .map_err(|_| "HandoffConfigurationUnavailable: invalid TOML".to_string())?;
    let mut providers = vec!["openai".to_string()];
    if let Some(table) = config
        .get("model_providers")
        .and_then(toml::Value::as_table)
    {
        providers.extend(table.keys().cloned());
    }
    let models = match fs::read(home.join("vellum-model-catalog.json")) {
        Ok(bytes) => {
            let catalog: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("HandoffCatalogInvalid: {e}"))?;
            catalog
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_owned))
                .collect::<Vec<_>>()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("HandoffCatalogUnavailable: {e}")),
    };
    Ok(
        json!({"runtimeReady": true, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "tools": tools, "providers": providers, "models": models}),
    )
}

pub(crate) fn start_with(
    paths: &AgentPaths,
    mut request: StartRequest,
    client: &dyn Client,
) -> Result<HandoffStatus, String> {
    validate(&request)?;
    if !paths.root.is_absolute() {
        return Err("HandoffPathUnsafe: state root must be absolute".into());
    }
    // No deployment writes when runtime readiness refuses the new handoff.
    client.preflight()?;
    let _guard = HostMutationGuard::acquire(&paths.mutation_lock)?;
    client.preflight()?;
    let prepare_only = request.prepare_only;
    request.prepare_only = false;
    let fingerprint = crate::operations::request_fingerprint(
        &serde_json::to_value(&request).map_err(|e| e.to_string())?,
    );
    let layout = Layout::new(paths, &request);
    let mut record = if record_path(paths, &request.operation_id)?
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        let record = load(paths, &request.operation_id)?;
        if record.fingerprint != fingerprint {
            return Err("OperationConflict: handoff request changed".into());
        }
        record
    } else {
        let record = Record {
            request,
            fingerprint,
            phase: Phase::Prepared,
            thread_id: None,
            turn_id: None,
            captures: BTreeMap::new(),
            capture_intents: BTreeMap::new(),
        };
        save(paths, &record)?;
        record
    };
    if matches!(record.phase, Phase::ThreadStarting | Phase::TurnStarting) {
        if !prepare_only {
            reconcile(paths, &mut record, &layout, client, true)?;
        }
        if matches!(record.phase, Phase::ThreadStarting | Phase::TurnStarting) {
            return Err(
                "HandoffStartIndeterminate: daemon evidence does not authorize replay".into(),
            );
        }
    }
    if record.phase == Phase::Running {
        return Ok(status(&record, &layout));
    }
    client.prepare_repo(&layout)?;
    if prepare_only {
        return Ok(status(&record, &layout));
    }
    if record.phase == Phase::Prepared {
        client.prepare_worktree(&layout, &record.request.snapshot_commit)?;
        record.phase = Phase::WorktreeReady;
        save(paths, &record)?;
    } else if matches!(
        record.phase,
        Phase::WorktreeReady | Phase::ThreadReady | Phase::ThreadAdopted
    ) {
        client.prepare_worktree(&layout, &record.request.snapshot_commit)?;
    }
    if record.phase == Phase::ThreadAdopted {
        // Never override the settings we are trying to qualify. A failed resume
        // leaves the adopted identity intact and cannot create a new thread.
        let response = client.call("thread/resume", json!({"threadId": record.thread_id}))?;
        if response.pointer("/thread/id").and_then(Value::as_str) != record.thread_id.as_deref()
            || response.get("cwd").and_then(Value::as_str) != layout.worktree.to_str()
            || response.pointer("/thread/cwd").and_then(Value::as_str) != layout.worktree.to_str()
        {
            return Err("HandoffStartIndeterminate: resumed thread identity/cwd changed".into());
        }
        qualify_configuration(&record, &layout, &response)?;
        // A dedicated recovered thread must still have no turn before we send
        // the first one. An unexpected turn requires observation, not replay.
        if !response
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .is_some_and(|turns| turns.is_empty())
        {
            return Err("HandoffStartIndeterminate: adopted thread already contains turns".into());
        }
        record.phase = Phase::ThreadReady;
        save(paths, &record)?;
    }
    if record.phase == Phase::WorktreeReady {
        record.phase = Phase::ThreadStarting;
        save(paths, &record)?;
        let response = client.call("thread/start", json!({
            "cwd": layout.worktree, "runtimeWorkspaceRoots": [layout.worktree],
            "model": record.request.model, "modelProvider": record.request.model_provider,
            "allowProviderModelFallback": false, "approvalPolicy": "never", "sandbox": "workspace-write",
            "ephemeral": false, "serviceName": "vellum-remote-handoff",
            "config": {"model_reasoning_effort": record.request.effort,
                "sandbox_workspace_write.network_access": false,
                "sandbox_workspace_write.exclude_tmpdir_env_var": true,
                "sandbox_workspace_write.exclude_slash_tmp": true},
        }))?;
        let id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|v| valid_id(v))
            .ok_or("HandoffResponseInvalid: thread id missing")?;
        record.thread_id = Some(id.into());
        // Save the identity even when qualification fails; never create a second thread.
        save(paths, &record)?;
        qualify_configuration(&record, &layout, &response)?;
        record.phase = Phase::ThreadReady;
        save(paths, &record)?;
    }
    if record.phase == Phase::ThreadReady {
        record.phase = Phase::TurnStarting;
        save(paths, &record)?;
        let response = client.call("turn/start", json!({
            "threadId": record.thread_id, "clientUserMessageId": record.request.operation_id,
            "cwd": layout.worktree, "model": record.request.model, "effort": record.request.effort,
            "approvalPolicy": "never",
            "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [layout.worktree], "networkAccess": false, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true},
            "input": [{"type": "text", "text": record.request.brief, "text_elements": []}],
        }))?;
        record.turn_id = Some(
            response
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .filter(|v| valid_id(v))
                .ok_or("HandoffResponseInvalid: turn id missing")?
                .into(),
        );
        record.phase = Phase::Running;
        save(paths, &record)?;
    }
    Ok(status(&record, &layout))
}

fn qualify_configuration(record: &Record, layout: &Layout, response: &Value) -> Result<(), String> {
    if response.get("model").and_then(Value::as_str) != Some(record.request.model.as_str())
        || response.get("modelProvider").and_then(Value::as_str)
            != Some(record.request.model_provider.as_str())
        || response.get("cwd").and_then(Value::as_str) != layout.worktree.to_str()
        || response.pointer("/sandbox/type").and_then(Value::as_str) != Some("workspaceWrite")
        || response.get("approvalPolicy").and_then(Value::as_str) != Some("never")
        || response.get("reasoningEffort").and_then(Value::as_str)
            != Some(record.request.effort.as_str())
        || response
            .pointer("/sandbox/excludeTmpdirEnvVar")
            .and_then(Value::as_bool)
            != Some(true)
        || response
            .pointer("/sandbox/excludeSlashTmp")
            .and_then(Value::as_bool)
            != Some(true)
        || response
            .pointer("/sandbox/networkAccess")
            .and_then(Value::as_bool)
            != Some(false)
        || response
            .pointer("/sandbox/writableRoots")
            .and_then(Value::as_array)
            .is_none_or(|roots| {
                roots
                    .iter()
                    .any(|path| path.as_str() != layout.worktree.to_str())
            })
    {
        return Err("HandoffConfigurationMismatch: daemon did not preserve model, cwd or workspace-write policy".into());
    }
    Ok(())
}

// Daemon calls here are observation only. The local record is advanced under
// the mutation lock; no resume, thread/start or turn/start is allowed here.
fn reconcile(
    paths: &AgentPaths,
    record: &mut Record,
    layout: &Layout,
    client: &dyn Client,
    explicit_start: bool,
) -> Result<(), String> {
    if record.phase == Phase::ThreadStarting {
        let mut cursor = Value::Null;
        let mut candidates = BTreeMap::new();
        // cwd is an exact filter in pinned v2. Bound paging and refuse adoption
        // unless the entire result set has been observed.
        for _ in 0..2 {
            let page = client.call(
                "thread/list",
                json!({
                    "cwd": layout.worktree, "cursor": cursor, "limit": 100,
                    "modelProviders": [], "useStateDbOnly": true,
                "sourceKinds": ["cli", "vscode", "exec", "appServer", "subAgent", "subAgentReview", "subAgentCompact", "subAgentThreadSpawn", "subAgentOther", "unknown"],
                }),
            )?;
            let threads = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or("HandoffResponseInvalid: thread/list missing data")?;
            for thread in threads {
                if thread.get("cwd").and_then(Value::as_str) == layout.worktree.to_str() {
                    let id = thread
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| valid_id(id))
                        .ok_or("HandoffResponseInvalid: thread/list invalid id")?;
                    candidates.insert(id.to_string(), ());
                }
            }
            if candidates.len() > 1 {
                return Ok(());
            }
            match page.get("nextCursor") {
                Some(Value::Null) => {
                    if let Some((id, ())) = candidates.into_iter().next() {
                        if record.thread_id.as_ref().is_none_or(|known| known == &id) {
                            record.thread_id = Some(id);
                            record.phase = Phase::ThreadAdopted;
                            save(paths, record)?;
                        }
                    } else if explicit_start && record.thread_id.is_none() {
                        // Coordinator's live 0.147.0-alpha.6.6 observation
                        // (remote-one-click-m3b-live-shape): thread/start creates
                        // only an in-memory shell until turn/start materializes
                        // it. No id was received, so we never sent a turn. A
                        // complete empty cwd listing permits a new empty shell;
                        // polling alone must not reset intent or send mutations.
                        // Keep known ids (including configuration failures).
                        record.phase = Phase::WorktreeReady;
                        save(paths, record)?;
                    }
                    return Ok(());
                }
                Some(Value::String(next)) if cursor.as_str() != Some(next) => cursor = json!(next),
                _ => return Err("HandoffResponseInvalid: thread/list invalid cursor".into()),
            }
        }
    } else if record.phase == Phase::TurnStarting {
        let response = match client.call(
            "thread/read",
            json!({"threadId": record.thread_id, "includeTurns": true}),
        ) {
            Ok(response) => response,
            Err(error) if not_materialized(&error) => {
                if explicit_start {
                    // Same pinned live observation: -32600 + this exact marker
                    // means no first user message/turn was materialized. Only
                    // the next explicit start may retry the original turn.
                    record.phase = Phase::ThreadReady;
                    save(paths, record)?;
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let thread = response
            .get("thread")
            .ok_or("HandoffResponseInvalid: thread missing")?;
        if thread.get("id").and_then(Value::as_str) != record.thread_id.as_deref()
            || thread.get("cwd").and_then(Value::as_str) != layout.worktree.to_str()
        {
            return Err("HandoffStartIndeterminate: observed thread identity/cwd changed".into());
        }
        let turns = thread
            .get("turns")
            .and_then(Value::as_array)
            .ok_or("HandoffResponseInvalid: turns missing")?;
        // Pinned 0.147 Turn has no clientUserMessageId. This is a new dedicated
        // handoff thread, so exactly one turn is the only unambiguous match.
        if turns.len() == 1 {
            let id = turns[0]
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| valid_id(id))
                .ok_or("HandoffResponseInvalid: turn id missing")?;
            record.turn_id = Some(id.into());
            record.phase = Phase::Running;
            save(paths, record)?;
        }
    }
    Ok(())
}

pub fn result(
    paths: &AgentPaths,
    home: &Path,
    request: ResultRequest,
) -> Result<HandoffStatus, String> {
    result_with(paths, request, &crate::handoff_git::NativeClient::new(home))
}

pub(crate) fn result_with(
    paths: &AgentPaths,
    request: ResultRequest,
    client: &dyn Client,
) -> Result<HandoffStatus, String> {
    record_path(paths, &request.handoff_id)?;
    if request.capture
        && request
            .operation_id
            .as_deref()
            .is_none_or(|id| !valid_id(id))
    {
        return Err("HandoffRequestInvalid: capture requires an operation id".into());
    }
    let initial = load(paths, &request.handoff_id)?;
    let _guard = if request.capture
        || matches!(initial.phase, Phase::ThreadStarting | Phase::TurnStarting)
    {
        Some(HostMutationGuard::acquire(&paths.mutation_lock)?)
    } else {
        None
    };
    let mut record = load(paths, &request.handoff_id)?;
    let layout = Layout::new(paths, &record.request);
    reconcile(paths, &mut record, &layout, client, false)?;
    let mut result = status(&record, &layout);
    if request.capture {
        if let Some(capture) = record
            .captures
            .get(request.operation_id.as_deref().unwrap_or_default())
        {
            result.capture = Some(capture.clone());
            result.state = if capture.partial {
                "capturedProgress"
            } else {
                "completed"
            }
            .into();
            result.turn_status = (!capture.partial).then(|| "completed".into());
            return Ok(result);
        }
    }
    if record.phase != Phase::Running {
        return Ok(result);
    }
    let response = client.call(
        "thread/read",
        json!({"threadId": record.thread_id, "includeTurns": true}),
    )?;
    let thread = response
        .get("thread")
        .ok_or("HandoffResponseInvalid: thread/read missing thread")?;
    if thread.get("id").and_then(Value::as_str) != record.thread_id.as_deref()
        || thread.get("cwd").and_then(Value::as_str) != layout.worktree.to_str()
    {
        return Err("HandoffResponseInvalid: remote thread identity/cwd changed".into());
    }
    let turn = thread
        .get("turns")
        .and_then(Value::as_array)
        .and_then(|turns| {
            turns
                .iter()
                .find(|turn| turn.get("id").and_then(Value::as_str) == record.turn_id.as_deref())
        })
        .ok_or("incompleteObservation: handoff turn is not observable")?;
    let turn_status = turn
        .get("status")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "inProgress" | "completed" | "interrupted" | "failed"))
        .ok_or("incompleteObservation: unknown handoff turn status")?;
    result.turn_status = Some(turn_status.into());
    result.state = if turn_status == "inProgress" {
        "running"
    } else {
        turn_status
    }
    .into();
    if let Some(items) = turn.get("items").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("plan") if result.plan.len() < 32 => result.plan.push(tail(
                    item.get("text").and_then(Value::as_str).unwrap_or_default(),
                    4096,
                )),
                Some("commandExecution") => {
                    result.recent_output = tail(
                        item.get("aggregatedOutput")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        16 * 1024,
                    )
                }
                Some("agentMessage") => {
                    result.summary = tail(
                        item.get("text").and_then(Value::as_str).unwrap_or_default(),
                        8192,
                    )
                }
                _ => {}
            }
        }
    }
    if request.capture {
        // A later turn in the same thread may still be editing. Never label its
        // snapshot final just because our original turn has completed.
        let active = thread.pointer("/status/type").and_then(Value::as_str) != Some("idle")
            || thread
                .get("turns")
                .and_then(Value::as_array)
                .is_some_and(|turns| {
                    turns
                        .iter()
                        .any(|t| t.get("status").and_then(Value::as_str) == Some("inProgress"))
                });
        let operation = request
            .operation_id
            .as_deref()
            .ok_or("HandoffRequestInvalid")?;
        let result_ref = format!(
            "refs/vellum-handoff-results/{}/{operation}",
            request.handoff_id
        );
        let partial = *record
            .capture_intents
            .entry(operation.into())
            .or_insert(active || turn_status != "completed");
        save(paths, &record)?;
        let capture = client.capture(
            &layout,
            &record.request.snapshot_commit,
            &result_ref,
            partial,
        )?;
        record.captures.insert(operation.into(), capture.clone());
        save(paths, &record)?;
        result.capture = Some(capture);
    }
    Ok(result)
}

fn tail(text: &str, limit: usize) -> String {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Fake {
        rpc: RefCell<Vec<(String, Value)>>,
        git: RefCell<Vec<String>>,
        refuse: Cell<bool>,
        fail_rpc: RefCell<Option<String>>,
        wrong_model: Cell<bool>,
        wrong_read_identity: Cell<bool>,
        turn_status: RefCell<String>,
        other_active: Cell<bool>,
        captures: RefCell<Vec<bool>>,
        fail_capture: Cell<bool>,
        list_candidates: Cell<usize>,
        wrong_resume_cwd: Cell<bool>,
        unmaterialized: Cell<bool>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                rpc: Default::default(),
                git: Default::default(),
                refuse: Cell::new(false),
                fail_rpc: Default::default(),
                wrong_model: Cell::new(false),
                wrong_read_identity: Cell::new(false),
                turn_status: RefCell::new("inProgress".into()),
                other_active: Cell::new(false),
                captures: Default::default(),
                fail_capture: Cell::new(false),
                list_candidates: Cell::new(0),
                wrong_resume_cwd: Cell::new(false),
                unmaterialized: Cell::new(false),
            }
        }
    }
    impl Client for Fake {
        fn preflight(&self) -> Result<(), String> {
            if self.refuse.get() {
                Err("codexRuntimeIncomplete: redeploy first".into())
            } else {
                Ok(())
            }
        }
        fn prepare_repo(&self, _: &Layout) -> Result<(), String> {
            self.git.borrow_mut().push("prepare repo".into());
            Ok(())
        }
        fn prepare_worktree(&self, _: &Layout, _: &str) -> Result<(), String> {
            self.git.borrow_mut().push("prepare worktree".into());
            Ok(())
        }
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.rpc.borrow_mut().push((method.into(), params.clone()));
            if self.fail_rpc.borrow().as_deref() == Some(method) {
                return Err("agent.transportTimeout".into());
            }
            match method {
                "thread/list" if self.unmaterialized.get() => {
                    Ok(json!({"data": [], "nextCursor": null}))
                }
                "thread/list" => Ok(
                    json!({"data": (0..self.list_candidates.get()).map(|i| json!({
                    "id": if i == 0 { "remote-thread".into() } else { format!("remote-thread-{i}") },
                    "cwd": params["cwd"], "modelProvider": "vellum",
                })).collect::<Vec<_>>(), "nextCursor": null}),
                ),
                "thread/resume" => {
                    if self.unmaterialized.get() {
                        return Err("HandoffDaemonRpcError: thread/resume: {\"code\":-32600,\"message\":\"no rollout found for thread id remote-thread\"}".into());
                    }
                    let started = self
                        .rpc
                        .borrow()
                        .iter()
                        .find(|(name, _)| name == "thread/start")
                        .unwrap()
                        .1
                        .clone();
                    Ok(
                        json!({"thread": {"id": if self.wrong_read_identity.get() { "wrong" } else { "remote-thread" }, "cwd": started["cwd"], "turns": []},
                        "cwd": if self.wrong_resume_cwd.get() { json!("/outside") } else { started["cwd"].clone() }, "model": if self.wrong_model.get() { json!("wrong") } else { started["model"].clone() },
                        "modelProvider": started["modelProvider"], "reasoningEffort": started["config"]["model_reasoning_effort"],
                        "approvalPolicy": "never", "sandbox": {"type": "workspaceWrite", "networkAccess": false,
                        "writableRoots": [], "excludeTmpdirEnvVar": true, "excludeSlashTmp": true}}),
                    )
                }
                "thread/start" => Ok(
                    json!({"thread": {"id": "remote-thread"}, "cwd": params["cwd"],
                    "model": if self.wrong_model.get() { json!("another-model") } else { params["model"].clone() },
                    "modelProvider": params["modelProvider"], "approvalPolicy": "never", "reasoningEffort": params["config"]["model_reasoning_effort"],
                    "sandbox": {"type": "workspaceWrite", "networkAccess": false, "writableRoots": [], "excludeTmpdirEnvVar": true, "excludeSlashTmp": true}}),
                ),
                "turn/start" => {
                    self.unmaterialized.set(false);
                    Ok(json!({"turn": {"id": "remote-turn", "status": "inProgress"}}))
                }
                "thread/read" => {
                    if self.unmaterialized.get() && params["includeTurns"] == true {
                        return Err(format!(
                            "HandoffDaemonRpcError: thread/read: {}",
                            json!({"code":-32600, "message":"thread remote-thread is not materialized yet; includeTurns is unavailable before first user message"})
                        ));
                    }
                    let cwd = self
                        .rpc
                        .borrow()
                        .iter()
                        .find(|(name, _)| name == "thread/start")
                        .unwrap()
                        .1["cwd"]
                        .clone();
                    let mut turns = vec![
                        json!({"id": "remote-turn", "status": *self.turn_status.borrow(), "items": [
                            {"type": "plan", "text": "1. Implement\n2. Test"},
                            {"type": "commandExecution", "aggregatedOutput": "tests passed"},
                            {"type": "agentMessage", "text": "Completed the task"}
                        ]}),
                    ];
                    if self.other_active.get() {
                        turns.push(json!({"id": "later-turn", "status": "inProgress"}));
                    }
                    Ok(
                        json!({"thread": {"id": if self.wrong_read_identity.get() { "another-thread" } else { "remote-thread" }, "cwd": cwd, "status": {"type": if self.other_active.get() || *self.turn_status.borrow() == "inProgress" { "active" } else { "idle" }}, "turns": turns}}),
                    )
                }
                _ => panic!("unexpected RPC: {method}"),
            }
        }
        fn capture(
            &self,
            _: &Layout,
            snapshot: &str,
            result_ref: &str,
            partial: bool,
        ) -> Result<Capture, String> {
            self.captures.borrow_mut().push(partial);
            if self.fail_capture.get() {
                return Err("injected snapshot failure".into());
            }
            Ok(Capture {
                result_ref: result_ref.into(),
                commit: "b".repeat(40),
                snapshot_commit: snapshot.into(),
                partial,
                diffstat: vec![FileStat {
                    path: "src/main.rs".into(),
                    added: Some(2),
                    deleted: Some(1),
                }],
            })
        }
    }
    fn request() -> StartRequest {
        StartRequest {
            operation_id: "handoff-1".into(),
            repo_id: "repo-1".into(),
            snapshot_commit: "a".repeat(40),
            source_thread_id: "local-thread".into(),
            brief: "Finish the remaining implementation and tests.\n\nBackground follows.".into(),
            model: "selected-model".into(),
            model_provider: "vellum".into(),
            effort: "medium".into(),
            prepare_only: false,
        }
    }
    fn result_request(capture: bool) -> ResultRequest {
        ResultRequest {
            handoff_id: "handoff-1".into(),
            capture,
            operation_id: capture.then(|| "capture-1".into()),
        }
    }

    #[test]
    fn runtime_refusal_and_invalid_paths_make_no_write_or_daemon_calls() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        fake.refuse.set(true);
        assert!(start_with(&paths, request(), &fake)
            .unwrap_err()
            .contains("codexRuntimeIncomplete"));
        assert!(!paths.root.exists());
        assert!(fake.git.borrow().is_empty());
        assert!(fake.rpc.borrow().is_empty());
        fake.refuse.set(false);
        let mut invalid = request();
        invalid.repo_id = "../outside".into();
        assert!(start_with(&paths, invalid, &fake).is_err());
        assert!(!paths.root.exists());
    }

    #[test]
    fn preflight_metadata_returns_only_public_provider_names_and_model_slugs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[model_providers.vellum]\nname='Vellum'\nexperimental_bearer_token='private-value'\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("vellum-model-catalog.json"),
            r#"{"models":[{"slug":"selected","description":"private-description"}]}"#,
        )
        .unwrap();
        let config_before = fs::read(temp.path().join("config.toml")).unwrap();
        let status = preflight_metadata(temp.path(), vec!["git".into(), "python3".into()]).unwrap();
        assert_eq!(status["providers"], json!(["openai", "vellum"]));
        assert_eq!(status["models"], json!(["selected"]));
        assert!(!status.to_string().contains("private"));
        assert_eq!(
            fs::read(temp.path().join("config.toml")).unwrap(),
            config_before
        );
    }

    #[test]
    fn git_preflight_requires_object_format_support_including_vendor_versions() {
        assert!(git_object_format_supported(
            "git version 2.39.5 (Apple Git-154)\n"
        ));
        assert!(git_object_format_supported(
            "git version 2.49.0.windows.1\n"
        ));
        assert!(!git_object_format_supported("git version 2.17.1\n"));
        assert!(!git_object_format_supported("unknown"));
    }

    #[test]
    fn prepare_push_start_and_replay_create_one_thread_and_one_turn_with_original_settings() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        let mut input = request();
        input.prepare_only = true;
        let prepared = start_with(&paths, input, &fake).unwrap();
        assert_eq!(prepared.state, "prepared");
        assert!(prepared.repo_path.ends_with("repo-1.git"));
        assert!(fake.rpc.borrow().is_empty());
        let started = start_with(&paths, request(), &fake).unwrap();
        assert_eq!(started.thread_id.as_deref(), Some("remote-thread"));
        assert_eq!(started.turn_id.as_deref(), Some("remote-turn"));
        assert_eq!(
            start_with(&paths, request(), &fake).unwrap().thread_id,
            started.thread_id
        );
        let rpc = fake.rpc.borrow();
        assert_eq!(
            rpc.iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["thread/start", "turn/start"]
        );
        assert_eq!(rpc[0].1["sandbox"], "workspace-write");
        assert_eq!(rpc[0].1["allowProviderModelFallback"], false);
        assert_eq!(rpc[1].1["effort"], "medium");
        assert_eq!(rpc[1].1["model"], "selected-model");
        assert_eq!(rpc[1].1["sandboxPolicy"]["networkAccess"], false);
        assert_eq!(rpc[1].1["input"][0]["text"], request().brief);
        drop(rpc);
        let mut changed = request();
        changed.brief = "different task".into();
        assert!(start_with(&paths, changed, &fake)
            .unwrap_err()
            .contains("OperationConflict"));
        assert_eq!(fake.rpc.borrow().len(), 2);
    }

    #[test]
    fn ambiguous_or_materialized_reply_never_repeats_the_mutation() {
        for method in ["thread/start", "turn/start"] {
            let temp = tempfile::tempdir().unwrap();
            let paths = AgentPaths::from_root(temp.path().join("state"));
            let fake = Fake::new();
            *fake.fail_rpc.borrow_mut() = Some(method.into());
            assert_eq!(
                start_with(&paths, request(), &fake).unwrap_err(),
                "agent.transportTimeout"
            );
            let calls = fake.rpc.borrow().len();
            if method == "thread/start" {
                fake.list_candidates.set(2);
                assert!(start_with(&paths, request(), &fake)
                    .unwrap_err()
                    .contains("HandoffStartIndeterminate"));
            } else {
                assert_eq!(
                    start_with(&paths, request(), &fake).unwrap().phase,
                    Phase::Running
                );
            }
            let observed = result_with(&paths, result_request(false), &fake).unwrap();
            assert_eq!(
                observed.state,
                if method == "turn/start" {
                    "running"
                } else {
                    "indeterminate"
                }
            );
            assert_eq!(observed.thread_id.is_some(), method == "turn/start");
            assert_eq!(
                fake.rpc
                    .borrow()
                    .iter()
                    .filter(|(name, _)| matches!(name.as_str(), "thread/start" | "turn/start"))
                    .count(),
                calls
            );
        }
    }

    #[test]
    fn lost_unmaterialized_thread_reply_can_replay_only_on_explicit_start() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        fake.unmaterialized.set(true);
        *fake.fail_rpc.borrow_mut() = Some("thread/start".into());
        assert!(start_with(&paths, request(), &fake).is_err());
        *fake.fail_rpc.borrow_mut() = None;
        let before = fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap();
        assert_eq!(
            result_with(&paths, result_request(false), &fake)
                .unwrap()
                .phase,
            Phase::ThreadStarting
        );
        assert_eq!(
            fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap(),
            before
        );
        assert_eq!(
            fake.rpc
                .borrow()
                .iter()
                .filter(|(name, _)| name == "thread/start")
                .count(),
            1
        );
        let recovered = start_with(&paths, request(), &fake).unwrap();
        assert_eq!(recovered.phase, Phase::Running);
        let rpc = fake.rpc.borrow();
        assert_eq!(
            rpc.iter()
                .filter(|(name, _)| name == "thread/start")
                .count(),
            2
        );
        assert_eq!(
            rpc.iter().filter(|(name, _)| name == "turn/start").count(),
            1
        );
        assert!(!rpc.iter().any(|(name, _)| name == "thread/resume"));
    }
    #[test]
    fn lost_unmaterialized_turn_reply_retries_same_thread_only_on_explicit_start() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        fake.unmaterialized.set(true);
        *fake.fail_rpc.borrow_mut() = Some("turn/start".into());
        assert!(start_with(&paths, request(), &fake).is_err());
        *fake.fail_rpc.borrow_mut() = None;
        let before = fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap();
        assert_eq!(
            result_with(&paths, result_request(false), &fake)
                .unwrap()
                .phase,
            Phase::TurnStarting
        );
        assert_eq!(
            fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap(),
            before
        );
        assert_eq!(
            fake.rpc
                .borrow()
                .iter()
                .filter(|(name, _)| name == "turn/start")
                .count(),
            1
        );
        assert!(fake
            .call("thread/resume", json!({"threadId":"remote-thread"}))
            .unwrap_err()
            .contains("no rollout"));
        assert_eq!(
            start_with(&paths, request(), &fake).unwrap().phase,
            Phase::Running
        );
        let rpc = fake.rpc.borrow();
        assert_eq!(
            rpc.iter()
                .filter(|(name, _)| name == "thread/start")
                .count(),
            1
        );
        assert_eq!(
            rpc.iter().filter(|(name, _)| name == "turn/start").count(),
            2
        );
        assert!(rpc
            .iter()
            .filter(|(name, _)| name == "turn/start")
            .all(|(_, params)| params["threadId"] == "remote-thread"));
    }
    #[test]
    fn not_materialized_requires_pinned_rpc_code_and_message_and_not_transport_text() {
        let rpc = |code, message| {
            format!(
                "HandoffDaemonRpcError: thread/read: {}",
                json!({"code":code,"message":message})
            )
        };
        assert!(not_materialized(&rpc(
            -32600,
            "thread is not materialized yet"
        )));
        assert!(!not_materialized(&rpc(
            -32601,
            "thread is not materialized yet"
        )));
        assert!(!not_materialized(&rpc(
            -32600,
            "no rollout found for thread"
        )));
        assert!(!not_materialized(
            "transport failure -32600 not materialized"
        ));
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        *fake.fail_rpc.borrow_mut() = Some("turn/start".into());
        assert!(start_with(&paths, request(), &fake).is_err());
        *fake.fail_rpc.borrow_mut() = Some("thread/read".into());
        assert_eq!(
            start_with(&paths, request(), &fake).unwrap_err(),
            "agent.transportTimeout"
        );
        assert_eq!(
            load(&paths, "handoff-1").unwrap().phase,
            Phase::TurnStarting
        );
        assert_eq!(
            fake.rpc
                .borrow()
                .iter()
                .filter(|(name, _)| name == "turn/start")
                .count(),
            1
        );
    }

    #[test]
    fn changed_model_refuses_to_start_a_turn_and_keeps_remote_thread_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        fake.wrong_model.set(true);
        assert!(start_with(&paths, request(), &fake)
            .unwrap_err()
            .contains("HandoffConfigurationMismatch"));
        let result = result_with(&paths, result_request(false), &fake).unwrap();
        assert_eq!(result.thread_id.as_deref(), Some("remote-thread"));
        assert_eq!(
            fake.rpc
                .borrow()
                .iter()
                .filter(|(name, _)| name == "thread/start")
                .count(),
            1
        );
    }

    #[test]
    fn lost_thread_reply_is_adopted_read_only_then_resumed_without_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        *fake.fail_rpc.borrow_mut() = Some("thread/start".into());
        assert!(start_with(&paths, request(), &fake).is_err());
        *fake.fail_rpc.borrow_mut() = None;
        fake.list_candidates.set(1);
        let adopted = result_with(&paths, result_request(true), &fake).unwrap();
        assert_eq!(adopted.phase, Phase::ThreadAdopted);
        assert_eq!(adopted.thread_id.as_deref(), Some("remote-thread"));
        assert!(adopted.capture.is_none());
        let count = fake.rpc.borrow().len();
        result_with(&paths, result_request(true), &fake).unwrap();
        assert_eq!(fake.rpc.borrow().len(), count);
        assert!(fake.captures.borrow().is_empty());
        start_with(&paths, request(), &fake).unwrap();
        let rpc = fake.rpc.borrow();
        assert_eq!(
            rpc.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["thread/start", "thread/list", "thread/resume", "turn/start"]
        );
        assert_eq!(rpc[2].1, json!({"threadId": "remote-thread"}));
    }

    #[test]
    fn adopted_resume_failure_mismatch_or_identity_change_never_starts_a_turn() {
        for failure in ["transport", "model", "identity", "cwd"] {
            let temp = tempfile::tempdir().unwrap();
            let paths = AgentPaths::from_root(temp.path().join("state"));
            let fake = Fake::new();
            *fake.fail_rpc.borrow_mut() = Some("thread/start".into());
            assert!(start_with(&paths, request(), &fake).is_err());
            *fake.fail_rpc.borrow_mut() = None;
            fake.list_candidates.set(1);
            result_with(&paths, result_request(false), &fake).unwrap();
            match failure {
                "transport" => *fake.fail_rpc.borrow_mut() = Some("thread/resume".into()),
                "model" => fake.wrong_model.set(true),
                "cwd" => fake.wrong_resume_cwd.set(true),
                _ => fake.wrong_read_identity.set(true),
            }
            let error = start_with(&paths, request(), &fake).unwrap_err();
            if failure == "model" {
                assert!(error.contains("HandoffConfigurationMismatch"));
            }
            let result = result_with(&paths, result_request(true), &fake).unwrap();
            assert_eq!(result.phase, Phase::ThreadAdopted);
            assert_eq!(result.thread_id.as_deref(), Some("remote-thread"));
            assert!(!fake
                .rpc
                .borrow()
                .iter()
                .any(|(name, _)| name == "turn/start"));
        }
    }

    #[test]
    fn multiple_thread_or_turn_candidates_remain_indeterminate_without_mutations() {
        for method in ["thread/start", "turn/start"] {
            let temp = tempfile::tempdir().unwrap();
            let paths = AgentPaths::from_root(temp.path().join("state"));
            let fake = Fake::new();
            *fake.fail_rpc.borrow_mut() = Some(method.into());
            assert!(start_with(&paths, request(), &fake).is_err());
            *fake.fail_rpc.borrow_mut() = None;
            fake.list_candidates.set(2);
            fake.other_active.set(true);
            let before = fake
                .rpc
                .borrow()
                .iter()
                .filter(|(name, _)| matches!(name.as_str(), "thread/start" | "turn/start"))
                .count();
            assert_eq!(
                result_with(&paths, result_request(true), &fake)
                    .unwrap()
                    .state,
                "indeterminate"
            );
            assert!(start_with(&paths, request(), &fake)
                .unwrap_err()
                .contains("HandoffStartIndeterminate"));
            assert_eq!(
                fake.rpc
                    .borrow()
                    .iter()
                    .filter(|(name, _)| matches!(name.as_str(), "thread/start" | "turn/start"))
                    .count(),
                before
            );
            assert!(fake.captures.borrow().is_empty());
        }
    }

    #[test]
    fn polling_is_read_only_and_partial_captures_remain_partial_across_retries() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        start_with(&paths, request(), &fake).unwrap();
        let record_before = fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap();
        let observed = result_with(&paths, result_request(false), &fake).unwrap();
        assert_eq!(observed.state, "running");
        assert_eq!(observed.plan, ["1. Implement\n2. Test"]);
        assert_eq!(observed.recent_output, "tests passed");
        assert!(fake.captures.borrow().is_empty());
        assert_eq!(
            fs::read(record_path(&paths, "handoff-1").unwrap()).unwrap(),
            record_before
        );
        fake.fail_capture.set(true);
        assert!(result_with(&paths, result_request(true), &fake).is_err());
        *fake.turn_status.borrow_mut() = "completed".into();
        fake.fail_capture.set(false);
        let capture = result_with(&paths, result_request(true), &fake)
            .unwrap()
            .capture
            .unwrap();
        assert!(capture.partial);
        assert_eq!(*fake.captures.borrow(), [true, true]);
        let calls = fake.rpc.borrow().len();
        assert_eq!(
            result_with(&paths, result_request(true), &fake)
                .unwrap()
                .capture
                .unwrap(),
            capture
        );
        assert_eq!(fake.rpc.borrow().len(), calls);
    }

    #[test]
    fn completed_results_are_final_only_when_no_later_turn_is_active() {
        for later in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let paths = AgentPaths::from_root(temp.path().join("state"));
            let fake = Fake::new();
            start_with(&paths, request(), &fake).unwrap();
            *fake.turn_status.borrow_mut() = "completed".into();
            fake.other_active.set(later);
            let result = result_with(&paths, result_request(true), &fake).unwrap();
            assert_eq!(result.capture.unwrap().partial, later);
        }
    }

    #[test]
    fn result_transport_failure_or_wrong_identity_never_captures_or_claims_completion() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let fake = Fake::new();
        start_with(&paths, request(), &fake).unwrap();
        *fake.fail_rpc.borrow_mut() = Some("thread/read".into());
        assert_eq!(
            result_with(&paths, result_request(true), &fake).unwrap_err(),
            "agent.transportTimeout"
        );
        *fake.fail_rpc.borrow_mut() = None;
        fake.wrong_read_identity.set(true);
        assert!(result_with(&paths, result_request(true), &fake)
            .unwrap_err()
            .contains("HandoffResponseInvalid"));
        assert!(fake.captures.borrow().is_empty());
    }

    #[test]
    fn protocol_roundtrips_flat_handoff_requests_and_rejects_policy_overrides() {
        let mut value = serde_json::to_value(request()).unwrap();
        value["method"] = "codex.handoffStart".into();
        let parsed: crate::protocol::AgentRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        value["sandbox"] = "danger-full-access".into();
        assert!(serde_json::from_value::<crate::protocol::AgentRequest>(value).is_err());
        let value = json!({"method": "codex.handoffResult", "handoffId": "handoff-1"});
        let parsed: crate::protocol::AgentRequest = serde_json::from_value(value).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();
        assert_eq!(serialized["method"], "codex.handoffResult");
        assert_eq!(serialized["capture"], false);
    }
}
