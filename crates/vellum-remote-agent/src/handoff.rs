//! Durable handoff orchestration. Only the existing daemon executes turns.
//! Persist intent before each non-idempotent RPC; an ambiguous reply never
//! authorizes another thread/start or turn/start.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mutation_lock::HostMutationGuard;
use crate::state::AgentPaths;

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
enum Phase {
    Prepared,
    WorktreeReady,
    ThreadStarting,
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
            Phase::ThreadStarting | Phase::TurnStarting => "indeterminate",
            Phase::Running => "running",
        }
        .into(),
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
        return Err("HandoffStartIndeterminate: inspect the recorded remote thread; do not repeat thread/start or turn/start".into());
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
    } else if matches!(record.phase, Phase::WorktreeReady | Phase::ThreadReady) {
        client.prepare_worktree(&layout, &record.request.snapshot_commit)?;
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
    let _guard = if request.capture {
        Some(HostMutationGuard::acquire(&paths.mutation_lock)?)
    } else {
        None
    };
    let mut record = load(paths, &request.handoff_id)?;
    let layout = Layout::new(paths, &record.request);
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
                "thread/start" => Ok(
                    json!({"thread": {"id": "remote-thread"}, "cwd": params["cwd"],
                    "model": if self.wrong_model.get() { json!("another-model") } else { params["model"].clone() },
                    "modelProvider": params["modelProvider"], "approvalPolicy": "never", "reasoningEffort": params["config"]["model_reasoning_effort"],
                    "sandbox": {"type": "workspaceWrite", "networkAccess": false, "writableRoots": [], "excludeTmpdirEnvVar": true, "excludeSlashTmp": true}}),
                ),
                "turn/start" => Ok(json!({"turn": {"id": "remote-turn", "status": "inProgress"}})),
                "thread/read" => {
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
    fn ambiguous_thread_or_turn_reply_never_repeats_the_mutation() {
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
            assert!(start_with(&paths, request(), &fake)
                .unwrap_err()
                .contains("HandoffStartIndeterminate"));
            let observed = result_with(&paths, result_request(false), &fake).unwrap();
            assert_eq!(observed.state, "indeterminate");
            assert_eq!(observed.thread_id.is_some(), method == "turn/start");
            assert_eq!(fake.rpc.borrow().len(), calls);
        }
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
        assert_eq!(fake.rpc.borrow().len(), 1);
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
