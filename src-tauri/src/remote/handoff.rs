//! Desktop-owned handoff state. Renderer inputs are thread IDs and a brief;
//! repository paths, model selection and SSH targets come from trusted stores.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::handoff_git::{Change, Git, Snapshot};
pub use super::handoff_threads::LocalThread;
use super::{RemoteAgentClient, RemoteHostManager};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[path = "handoff_result.rs"]
mod result;
pub use result::{acknowledge, capture, discard, review, settle, ResultState, Review};
#[path = "handoff_desktop.rs"]
mod desktop;
pub use desktop::thread_link;
#[cfg(test)]
#[path = "handoff_transport_tests.rs"]
mod transport_tests;

fn error(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}
pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Requirements {
    pub required_os: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    #[serde(default)]
    pub auto_writeback: bool,
    pub thread: LocalThread,
    pub changes: Vec<Change>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub remote_os: Option<String>,
    pub local_ref_notice: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub writeback: Option<crate::enhanced_runtime::handoff_inject::WritebackStatus>,
    pub id: String,
    pub host_id: String,
    pub source: LocalThread,
    pub snapshot: Snapshot,
    pub request: Value,
    pub stage: String,
    pub remote: Option<Value>,
    pub error: Option<String>,
    #[serde(default)]
    pub result: ResultState,
}

trait Transport {
    fn rpc(&self, request: Value) -> AppResult<Value>;
    fn push(
        &self,
        source: &Path,
        snapshot: &Snapshot,
        repo: &str,
        reference: &str,
    ) -> AppResult<()>;
    fn fetch(&self, source: &Path, repo: &str, reference: &str, commit: &str) -> AppResult<()> {
        let _ = (source, repo, reference, commit);
        Err(error("HandoffFetchUnavailable"))
    }
}
struct SshTransport {
    client: RemoteAgentClient,
    root: PathBuf,
}
impl Transport for SshTransport {
    fn fetch(&self, source: &Path, repo: &str, reference: &str, commit: &str) -> AppResult<()> {
        let alias = self
            .client
            .target()
            .ssh_destination
            .as_deref()
            .ok_or_else(|| error("HandoffManagedSshRequired"))?;
        Git::new(source, &self.root).fetch(&self.root, alias, repo, reference, commit)
    }
    fn rpc(&self, request: Value) -> AppResult<Value> {
        if request["method"] == "agent.version" {
            self.client.agent_version()
        } else {
            self.client.rpc(request)
        }
    }
    fn push(
        &self,
        source: &Path,
        snapshot: &Snapshot,
        repo: &str,
        reference: &str,
    ) -> AppResult<()> {
        let alias = self
            .client
            .target()
            .ssh_destination
            .as_deref()
            .ok_or_else(|| error("HandoffManagedSshRequired"))?;
        Git::new(source, &self.root).push(&self.root, alias, repo, snapshot, reference)
    }
}
fn transport(state: &AppState, host: &str) -> AppResult<SshTransport> {
    transport_with_home(state, host, dirs::home_dir().as_deref())
}

fn transport_with_home(
    state: &AppState,
    host: &str,
    home: Option<&Path>,
) -> AppResult<SshTransport> {
    let mut target = RemoteHostManager::resolve_target(state, host)?;
    let config = if host.starts_with("added-") {
        Some(super::ssh_added::config_path(&state.data_root()))
    } else {
        home.map(|home| home.join(".ssh/config"))
    };
    // Handoff uses the dedicated key/wrapper, preserving CODEX_HOME isolation.
    // Discovery keeps the user's alias for display and ordinary RPCs. Authority
    // comes from bootstrap's actual marked config block, never the cached name.
    if !config
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|contents| super::ssh_isolation::has_managed_alias(&contents, host))
    {
        return Err(error(
            "HandoffManagedSshRequired: deploy managed SSH isolation first",
        ));
    }
    // RPC, push and fetch all read this same target. Do not persist this alias
    // into discovery/cache, where the next refresh would replace it again.
    target.ssh_destination = Some(super::ssh_isolation::managed_alias(host));
    Ok(SshTransport {
        client: RemoteAgentClient::new(target),
        root: state.data_root(),
    })
}

pub fn list_threads(state: &AppState) -> AppResult<Vec<LocalThread>> {
    let home = crate::codex::CodexPaths::discover(&state.data_root())
        .config
        .parent()
        .ok_or_else(|| error("HandoffCodexHomeUnavailable"))?
        .to_path_buf();
    let mut threads = super::handoff_threads::discover(&home, &state.data_root())?;
    let records = list(state)?;
    for thread in &mut threads {
        thread.handed_off = records.iter().any(|record| {
            record.result.outcome.is_none()
                && crate::codex::conversation_key_matches(
                    &thread.thread_id,
                    &record.source.thread_id,
                )
        });
    }
    Ok(threads)
}
fn selected_thread(state: &AppState, id: &str) -> AppResult<LocalThread> {
    let root = state.data_root();
    let home = crate::codex::CodexPaths::discover(&root)
        .config
        .parent()
        .ok_or_else(|| error("HandoffCodexHomeUnavailable"))?
        .to_path_buf();
    let mut matches = super::handoff_threads::select(&home, &root, id)?
        .into_iter()
        .filter(|thread| crate::codex::conversation_key_matches(id, &thread.thread_id));
    let mut selected = matches
        .next()
        .ok_or_else(|| error("HandoffThreadUnavailable"))?;
    if matches.next().is_some() {
        return Err(error("HandoffThreadIdentityAmbiguous"));
    }
    selected.handed_off = list(state)?.iter().any(|record| {
        record.result.outcome.is_none()
            && crate::codex::conversation_key_matches(&selected.thread_id, &record.source.thread_id)
    });
    Ok(selected)
}

pub fn preflight(
    state: &AppState,
    host: &str,
    thread: &str,
    requirements: &Requirements,
) -> AppResult<Preflight> {
    let source = selected_thread(state, thread)?;
    let local = local_preflight(&state.data_root(), source)?;
    if !local.blockers.is_empty() {
        return Ok(local);
    }
    let mut report = inspect(
        local.thread,
        local.changes,
        requirements,
        &transport(state, host)?,
    )?;
    report.auto_writeback =
        crate::enhanced_runtime::handoff_inject::supports_thread(&state.data_root(), thread);
    Ok(report)
}

fn new_report(source: LocalThread, changes: Vec<Change>) -> Preflight {
    Preflight {
        auto_writeback: false,
        thread: source, changes, blockers: Vec::new(), warnings: Vec::new(), remote_os: None,
        local_ref_notice: "Handoff creates refs/vellum-handoff/<id> in this repository. The ref keeps the snapshot reachable until the result is safely applied or saved as a branch, or you discard the handoff.".into(),
    }
}
fn local_preflight(root: &Path, source: LocalThread) -> AppResult<Preflight> {
    let mut report = new_report(source, Vec::new());
    match Git::new(&report.thread.repo, root).inspect() {
        Ok((_, changes)) => report.changes = changes,
        Err(e) if e.to_string().starts_with("HandoffLfsUnsupported") => {
            report.blockers.push(e.to_string())
        }
        Err(e) => return Err(e),
    }
    Ok(report)
}

fn inspect(
    source: LocalThread,
    changes: Vec<Change>,
    requirements: &Requirements,
    client: &dyn Transport,
) -> AppResult<Preflight> {
    let mut report = new_report(source, changes);
    if report.thread.handed_off {
        report
            .blockers
            .push("HandoffThreadAlreadyHandedOff: inspect its existing handoff record".into());
    }
    if report.thread.active == Some(true) {
        report
            .blockers
            .push("activeWorkInProgress: finish or interrupt the local turn before handoff".into());
    }
    if report.thread.active.is_none() {
        report
            .blockers
            .push("incompleteObservation: local turn activity could not be observed".into());
    }
    if [&report.thread.model, &report.thread.model_provider]
        .iter()
        .any(|value| {
            value.as_deref().is_none_or(|text| {
                text.is_empty() || text.len() > 256 || text.chars().any(char::is_control)
            })
        })
        || !matches!(
            report.thread.effort.as_deref(),
            Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra")
        )
    {
        report.blockers.push("HandoffThreadSettingsUnavailable: the last turn's model/provider/effort must be observable".into());
    }
    if requirements
        .required_os
        .as_deref()
        .is_some_and(|os| !matches!(os, "windows" | "linux" | "darwin"))
        || requirements.tools.iter().any(|tool| {
            ![
                "git", "python", "python3", "node", "pnpm", "cargo", "dotnet",
            ]
            .contains(&tool.as_str())
        })
    {
        return Err(error("HandoffRequirementsInvalid"));
    }
    let version = client.rpc(json!({"method": "agent.version"}))?;
    if version["codexHandoffPreflight"] != true {
        report
            .blockers
            .push("RemoteCodexAgentUpdateRequired: redeploy the Agent before handoff".into());
        return Ok(report);
    }
    let host = client.rpc(json!({"method": "host.status"}))?;
    let native = &host["nativeCodex"];
    if native["compatible"] != true
        || native["daemonRunning"] != true
        || native["restartSafe"] != true
    {
        report.blockers.push(
            "HandoffDaemonUnavailable: deploy a compatible managed Codex daemon first".into(),
        );
    }
    if native.pointer("/runtimePackage/complete") != Some(&json!(true)) {
        report
            .blockers
            .push("codexRuntimeIncomplete: redeploy the complete Codex runtime".into());
    }
    // Native preflight remains authoritative and all transport failures propagate.
    // Skip it only when observed host facts already refuse this handoff.
    if !report.blockers.is_empty() {
        return Ok(report);
    }
    let probe = client.rpc(json!({"method": "codex.handoffPreflight"}))?;
    report.remote_os = probe["os"].as_str().map(str::to_owned);
    if probe["runtimeReady"] != true {
        report.blockers.push("HandoffDaemonUnavailable".into());
    }
    let os = probe["os"]
        .as_str()
        .ok_or_else(|| error("HandoffPreflightInvalid: missing OS"))?;
    let arch = probe["arch"]
        .as_str()
        .ok_or_else(|| error("HandoffPreflightInvalid: missing architecture"))?;
    if let Err(reason) = super::platform::RemotePlatform::from_os_arch(os, arch) {
        report
            .blockers
            .push(format!("{}: {}", reason.code, reason.message));
    }
    let detected_windows = fs::read_dir(&report.thread.repo)
        .map_err(|e| error(e.to_string()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "csproj"))
        .any(|entry| {
            fs::read_to_string(entry.path()).is_ok_and(|text| {
                text.contains("-windows")
                    || text.contains("<UseWPF>true</UseWPF>")
                    || text.contains("<UseWindowsForms>true</UseWindowsForms>")
            })
        });
    let required_os = requirements
        .required_os
        .as_deref()
        .or(detected_windows.then_some("windows"));
    if required_os.is_some_and(|required| required != os) {
        report.blockers.push(format!(
            "HandoffPlatformMismatch: task requires {}, remote is {os}",
            required_os.unwrap_or_default()
        ));
    }
    let tools = probe["tools"]
        .as_array()
        .ok_or_else(|| error("HandoffPreflightInvalid: missing tools"))?;
    if !tools.iter().any(|v| v == "git") {
        report
            .blockers
            .push("HandoffGitUnavailable: install Git on this host".into());
    }
    if probe["gitObjectFormatSupported"] != true {
        report
            .blockers
            .push("HandoffGitVersionUnsupported: upgrade remote Git to 2.29 or newer".into());
    }
    let mut requested_tools = requirements
        .tools
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for (marker, tool) in [
        ("Cargo.toml", "cargo"),
        ("package.json", "node"),
        ("pnpm-lock.yaml", "pnpm"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
    ] {
        if report.thread.repo.join(marker).is_file() {
            requested_tools.insert(tool.into());
        }
    }
    if detected_windows {
        requested_tools.insert("dotnet".into());
    }
    for tool in &requested_tools {
        if !tools.iter().any(|v| v.as_str() == Some(tool.as_str())) {
            let detail = if tool == "python" && tools.iter().any(|v| v == "python3") {
                "use python3 instead"
            } else {
                "install it or adjust the task commands"
            };
            report
                .warnings
                .push(format!("Remote tool {tool} is unavailable; {detail}."));
        }
    }
    let provider = report.thread.model_provider.as_deref().unwrap_or_default();
    let providers = probe["providers"]
        .as_array()
        .ok_or_else(|| error("HandoffPreflightInvalid: missing providers"))?;
    if !providers
        .iter()
        .any(|value| value.as_str() == Some(provider))
    {
        report.blockers.push(format!(
            "HandoffModelUnavailable: provider {provider} is not deployed"
        ));
    }
    if provider == crate::codex::VELLUM_PROVIDER_NAME {
        let models = probe["models"]
            .as_array()
            .ok_or_else(|| error("HandoffPreflightInvalid: missing models"))?;
        if !models
            .iter()
            .any(|value| value.as_str() == report.thread.model.as_deref())
        {
            report.blockers.push(
                "HandoffModelUnavailable: deploy the local thread's model on this host".into(),
            );
        }
        if host.pointer("/proxy/ready") != Some(&json!(true))
            || host.pointer("/configuration/credentialsReady") != Some(&json!(true))
        {
            report.blockers.push(
                "HandoffHostNotReady: converge proxy configuration and credentials first".into(),
            );
        }
    } else if matches!(
        provider,
        "openai" | crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME
    ) && host.pointer("/chatgpt/state").and_then(Value::as_str) != Some("ready")
    {
        report
            .blockers
            .push("officialAccountPairingRequired: pair and activate ChatGPT on this host".into());
    }
    Ok(report)
}

pub fn start(
    state: &AppState,
    host: &str,
    thread: &str,
    brief: &str,
    requirements: &Requirements,
) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, "new-handoff")?;
    let source = selected_thread(state, thread)?;
    let client = transport(state, host)?;
    begin(&root, host, source, brief, requirements, &client)
}
fn begin(
    root: &Path,
    host: &str,
    source: LocalThread,
    brief: &str,
    requirements: &Requirements,
    client: &dyn Transport,
) -> AppResult<Record> {
    if brief.trim().is_empty() || brief.len() > 120 * 1024 {
        return Err(error(
            "HandoffBriefInvalid: put the task in the first paragraph",
        ));
    }
    let git = Git::new(&source.repo, root);
    let local = local_preflight(root, source.clone())?;
    if !local.blockers.is_empty() {
        return Err(error(format!(
            "HandoffPreflightBlocked: {}",
            local.blockers.join("; ")
        )));
    }
    let report = inspect(source.clone(), local.changes, requirements, client)?;
    if !report.blockers.is_empty() {
        return Err(error(format!(
            "HandoffPreflightBlocked: {}",
            report.blockers.join("; ")
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let snapshot = git.snapshot(&id)?;
    let repo_id = hex::encode(Sha256::digest(source.repo.to_string_lossy().as_bytes()));
    let mut brief = brief.trim().to_owned();
    if !report.warnings.is_empty() {
        brief.push_str("\n\nRemote toolchain notes:\n");
        brief.push_str(&report.warnings.join("\n"));
    }
    let request = json!({"method": "codex.handoffStart", "operationId": id, "repoId": repo_id,
        "snapshotCommit": snapshot.commit, "sourceThreadId": source.thread_id, "brief": brief,
        "model": source.model, "modelProvider": source.model_provider, "effort": source.effort, "prepareOnly": true});
    let mut record = Record {
        writeback: None,
        id,
        host_id: host.into(),
        source,
        snapshot,
        request,
        stage: "localSnapshot".into(),
        remote: None,
        error: None,
        result: ResultState::default(),
    };
    let saved = (|| -> AppResult<Lock> {
        let guard = Lock::acquire(root, &record.id)?;
        save(root, &record)?;
        Ok(guard)
    })();
    let _handoff_guard = match saved {
        Ok(guard) => guard,
        Err(e) => {
            if let Err(cleanup) = git.release_snapshot(&record.snapshot) {
                return Err(error(format!(
                    "{e}; snapshot ref {} retained: {cleanup}",
                    record.snapshot.local_ref
                )));
            }
            return Err(e);
        }
    };
    drive(root, &mut record, client)?;
    Ok(record)
}

fn validate_prepared(record: &Record, remote: &Value) -> AppResult<(String, String)> {
    let repo = remote["repoPath"]
        .as_str()
        .ok_or_else(|| error("HandoffPreparedResponseInvalid"))?;
    let suffix = format!(
        "/handoffs/repos/{}.git",
        record.request["repoId"].as_str().unwrap_or_default()
    );
    let prefix = repo
        .strip_suffix(&suffix)
        .filter(|prefix| prefix.starts_with('/') && !prefix.contains(['\\', '\0', '\n', '\r']))
        .ok_or_else(|| error("HandoffPreparedPathInvalid"))?;
    if prefix.split('/').any(|part| matches!(part, "." | ".."))
        || remote["handoffId"].as_str() != Some(record.id.as_str())
        || remote["sourceThreadId"].as_str() != Some(record.source.thread_id.as_str())
        || remote["worktree"].as_str()
            != Some(format!("{prefix}/handoffs/work/{}", record.id).as_str())
        || remote["snapshotRef"].as_str()
            != Some(format!("refs/vellum-handoff/{}", record.id).as_str())
    {
        return Err(error("HandoffPreparedIdentityMismatch"));
    }
    Ok((repo.into(), format!("refs/vellum-handoff/{}", record.id)))
}
fn drive(root: &Path, record: &mut Record, client: &dyn Transport) -> AppResult<()> {
    let result = (|| {
        if record.stage == "localSnapshot" {
            let mut request = record.request.clone();
            request["prepareOnly"] = true.into();
            let prepared = client.rpc(request)?;
            validate_prepared(record, &prepared)?;
            record.remote = Some(prepared);
            record.stage = "remotePrepared".into();
            save(root, record)?;
        }
        if record.stage == "remotePrepared" {
            let (repo, reference) = validate_prepared(
                record,
                record
                    .remote
                    .as_ref()
                    .ok_or_else(|| error("HandoffPreparedResponseInvalid"))?,
            )?;
            client.push(&record.source.repo, &record.snapshot, &repo, &reference)?;
            record.stage = "snapshotPushed".into();
            save(root, record)?;
        }
        // Only begin/explicit resume call drive. Poll remains observation-only.
        // The Agent owns evidence-based replay of pinned unmaterialized RPCs.
        if matches!(
            record.stage.as_str(),
            "snapshotPushed" | "adopted" | "startUncertain"
        ) {
            record.stage = "startUncertain".into();
            save(root, record)?;
            let mut request = record.request.clone();
            request["prepareOnly"] = false.into();
            let started = client.rpc(request)?;
            update_remote(record, started)?;
        }
        record.error = None;
        save(root, record)
    })();
    if let Err(ref e) = result {
        record.error = Some(e.to_string());
        save(root, record)?;
    }
    result
}
fn update_remote(record: &mut Record, remote: Value) -> AppResult<()> {
    validate_prepared(record, &remote)?;
    let consistent = match remote["phase"].as_str() {
        Some("prepared" | "worktreeReady" | "threadReady") => remote["state"] == "prepared",
        Some("threadStarting" | "threadAdopted" | "turnStarting") => {
            remote["state"] == "indeterminate"
        }
        Some("running") => matches!(
            remote["state"].as_str(),
            Some("running" | "completed" | "failed" | "interrupted")
        ),
        _ => false,
    };
    if !consistent {
        return Err(error(
            "HandoffResponseIdentityInvalid: phase/state disagree",
        ));
    }
    for field in ["threadId", "turnId"] {
        if let Some(known) = record
            .remote
            .as_ref()
            .and_then(|value| value[field].as_str())
        {
            if remote[field].as_str() != Some(known) {
                return Err(error(
                    "HandoffResponseIdentityInvalid: remote identity changed",
                ));
            }
        }
    }
    if remote["phase"] == "running"
        && ["threadId", "turnId"]
            .iter()
            .any(|field| !remote[*field].as_str().is_some_and(valid_id))
    {
        return Err(error(
            "HandoffResponseIdentityInvalid: missing thread/turn id",
        ));
    }
    record.stage = if remote["phase"] == "threadAdopted" {
        "adopted"
    } else if remote["state"] == "indeterminate" {
        "startUncertain"
    } else if remote["state"] == "prepared" {
        "snapshotPushed"
    } else {
        "running"
    }
    .into();
    record.remote = Some(remote);
    Ok(())
}

pub fn resume(state: &AppState, id: &str) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    if record.result.outcome.is_some() {
        return Ok(record);
    }
    let client = transport(state, &record.host_id)?;
    resume_with(&root, &mut record, &client)?;
    Ok(record)
}
fn resume_with(root: &Path, record: &mut Record, client: &dyn Transport) -> AppResult<()> {
    if matches!(
        record.stage.as_str(),
        "startUncertain" | "adopted" | "running"
    ) {
        let remote =
            client.rpc(json!({"method": "codex.handoffResult", "handoffId": record.id}))?;
        update_remote(record, remote)?;
        save(root, record)?;
    }
    drive(root, record, client)
}
pub fn poll(state: &AppState, id: &str) -> AppResult<Record> {
    let root = state.data_root();
    let _guard = Lock::acquire(&root, id)?;
    let mut record = load(&root, id)?;
    if record.result.outcome.is_some() {
        return Ok(record);
    }
    let client = transport(state, &record.host_id)?;
    poll_with(&root, &mut record, &client)?;
    Ok(record)
}
fn poll_with(root: &Path, record: &mut Record, client: &dyn Transport) -> AppResult<()> {
    // Remote preparation does not prove the snapshot was pushed. Observation
    // must never skip a pending transfer, or start a turn by itself.
    if matches!(record.stage.as_str(), "localSnapshot" | "remotePrepared") {
        return Ok(());
    }
    let result = client.rpc(json!({"method": "codex.handoffResult", "handoffId": record.id}));
    match result {
        Ok(remote) => {
            update_remote(record, remote)?;
            record.error = None;
        }
        Err(e) => {
            record.error = Some(e.to_string());
            save(root, record)?;
            return Err(e);
        }
    }
    save(root, record)
}
pub fn list(state: &AppState) -> AppResult<Vec<Record>> {
    list_at(&state.data_root())
}
fn list_at(root: &Path) -> AppResult<Vec<Record>> {
    let directory = root.join("remote-handoffs/records");
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| error(e.to_string()))? {
        let entry = entry.map_err(|e| error(e.to_string()))?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            let id = entry
                .path()
                .file_stem()
                .and_then(|id| id.to_str())
                .ok_or_else(|| error("HandoffRecordInvalid"))?
                .to_owned();
            let mut record = load(root, &id)?;
            record.writeback = crate::enhanced_runtime::handoff_inject::writeback_status(root, &id);
            if record
                .remote
                .as_ref()
                .is_some_and(|remote| remote["state"] == "completed")
            {
                if let Err(error) =
                    crate::enhanced_runtime::handoff_inject::recover(root, &record.id)
                {
                    log::debug!("[HandoffInject] request recovery failed: {error}");
                }
            }
            records.push(record);
        }
    }
    records.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(records)
}
fn path(root: &Path, id: &str) -> AppResult<PathBuf> {
    if !valid_id(id) {
        return Err(error("HandoffIdInvalid"));
    }
    Ok(root
        .join("remote-handoffs/records")
        .join(format!("{id}.json")))
}
fn load(root: &Path, id: &str) -> AppResult<Record> {
    let bytes = fs::read(path(root, id)?).map_err(|e| error(e.to_string()))?;
    let record: Record = serde_json::from_slice(&bytes).map_err(|e| error(e.to_string()))?;
    if record.id != id {
        return Err(error("HandoffRecordIdentityMismatch"));
    }
    Ok(record)
}
fn save(root: &Path, record: &Record) -> AppResult<()> {
    let path = path(root, &record.id)?;
    fs::create_dir_all(path.parent().ok_or_else(|| error("HandoffPathInvalid"))?)
        .map_err(|e| error(e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| error(e.to_string()))?;
    // NamedTempFile::persist replaces atomically on Windows too; never remove
    // an existing journal before publishing its replacement.
    let parent = path.parent().ok_or_else(|| error("HandoffPathInvalid"))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| error(e.to_string()))?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|e| error(e.to_string()))?;
    temporary.persist(&path).map_err(|e| error(e.to_string()))?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| error(e.to_string()))?;
    if record
        .remote
        .as_ref()
        .is_some_and(|remote| remote["state"] == "completed")
    {
        if let Err(error) = crate::enhanced_runtime::handoff_inject::publish(root, &record.id) {
            log::debug!("[HandoffInject] request could not be published: {error}");
        }
    }
    Ok(())
}
struct Lock(File);
impl Lock {
    fn acquire(root: &Path, id: &str) -> AppResult<Self> {
        if !valid_id(id) {
            return Err(error("HandoffIdInvalid"));
        }
        let directory = root.join("remote-handoffs/locks");
        fs::create_dir_all(&directory).map_err(|e| error(e.to_string()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(id))
            .map_err(|e| error(e.to_string()))?;
        file.try_lock_exclusive()
            .map_err(|_| error("HandoffOperationBusy: wait for the current operation"))?;
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        calls: RefCell<Vec<Value>>,
        incomplete: Cell<bool>,
        fail_start: Cell<bool>,
        fail_push: Cell<bool>,
        result: RefCell<Value>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                calls: Default::default(),
                incomplete: Cell::new(false),
                fail_start: Cell::new(false),
                fail_push: Cell::new(false),
                result: RefCell::new(json!({"state": "indeterminate", "phase": "threadStarting"})),
            }
        }
    }
    fn wire_status(request: &Value) -> Value {
        json!({"handoffId": request["operationId"], "sourceThreadId": request["sourceThreadId"],
            "state": "prepared", "phase": "prepared",
            "repoPath": format!("/state/handoffs/repos/{}.git", request["repoId"].as_str().unwrap()),
            "worktree": format!("/state/handoffs/work/{}", request["operationId"].as_str().unwrap()),
            "snapshotRef": format!("refs/vellum-handoff/{}", request["operationId"].as_str().unwrap())})
    }
    impl Transport for Fake {
        fn rpc(&self, request: Value) -> AppResult<Value> {
            self.calls.borrow_mut().push(request.clone());
            match request["method"].as_str().unwrap() {
                "agent.version" => Ok(json!({"codexHandoffPreflight": true})),
                "host.status" => Ok(
                    json!({"nativeCodex": {"compatible": true, "daemonRunning": true, "restartSafe": true,
                    "runtimePackage": {"complete": !self.incomplete.get()}}, "proxy": {"ready": true}, "configuration": {"credentialsReady": true}}),
                ),
                "codex.handoffPreflight" => Ok(
                    json!({"runtimeReady": true, "os": "linux", "arch": "x86_64", "tools": ["git", "python3"], "gitObjectFormatSupported": true, "providers": ["vellum"], "models": ["model"]}),
                ),
                "codex.handoffStart" if request["prepareOnly"] == true => Ok(wire_status(&request)),
                "codex.handoffStart" if self.fail_start.get() => {
                    Err(error("ssh transport timeout"))
                }
                "codex.handoffStart" => {
                    let mut status = wire_status(&request);
                    status["state"] = "running".into();
                    status["phase"] = "running".into();
                    status["threadId"] = "remote-thread".into();
                    status["turnId"] = "remote-turn".into();
                    Ok(status)
                }
                "codex.handoffResult" => {
                    let prepared = self
                        .calls
                        .borrow()
                        .iter()
                        .find(|value| value["method"] == "codex.handoffStart")
                        .unwrap()
                        .clone();
                    let mut status = wire_status(&prepared);
                    status
                        .as_object_mut()
                        .unwrap()
                        .extend(self.result.borrow().as_object().unwrap().clone());
                    Ok(status)
                }
                method => panic!("unexpected request {method}"),
            }
        }
        fn push(
            &self,
            _: &Path,
            snapshot: &Snapshot,
            repo: &str,
            reference: &str,
        ) -> AppResult<()> {
            self.calls.borrow_mut().push(json!({"method": "push", "commit": snapshot.commit, "repo": repo, "reference": reference}));
            if self.fail_push.get() {
                return Err(error("injected push failure"));
            }
            Ok(())
        }
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, LocalThread) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let root = temp.path().join("data");
        let git = Git::new(&repo, &root);
        git.run(&["init"], None).unwrap();
        fs::write(repo.join("file"), "base\n").unwrap();
        git.run(&["add", "."], None).unwrap();
        git.run(&["commit", "-m", "base"], None).unwrap();
        fs::write(repo.join("file"), "unfinished\n").unwrap();
        let source = LocalThread {
            thread_id: "local-thread".into(),
            name: "Continue task".into(),
            repo,
            branch: "main".into(),
            updated_at: 1,
            model: Some("model".into()),
            model_provider: Some("vellum".into()),
            effort: Some("high".into()),
            active: Some(false),
            handed_off: false,
            draft: None,
        };
        (temp, root, source)
    }
    #[test]
    fn completed_journal_publishes_inbox_and_publish_failure_does_not_fail_save() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        let mut record = begin(
            &root,
            "host",
            source,
            "Finish task",
            &Requirements::default(),
            &client,
        )
        .unwrap();
        let inbox = root.join("enhanced-runtime/handoff-inject");
        assert!(!inbox.exists());
        record.remote = Some(json!({"state":"completed", "summary":"Done"}));
        save(&root, &record).unwrap();
        assert!(inbox.join(format!("{}.request", record.id)).exists());
        assert_eq!(
            load(&root, &record.id).unwrap().remote.unwrap()["state"],
            "completed"
        );
        fs::remove_file(inbox.join(format!("{}.request", record.id))).unwrap();
        list_at(&root).unwrap();
        assert!(inbox.join(format!("{}.request", record.id)).exists());
        fs::remove_dir_all(&inbox).unwrap();
        fs::write(&inbox, b"injected write failure").unwrap();
        save(&root, &record).unwrap();
        assert_eq!(
            load(&root, &record.id).unwrap().remote.unwrap()["summary"],
            "Done"
        );
    }

    #[test]
    fn prepare_push_start_preserves_settings_and_includes_toolchain_notes() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        let record = begin(
            &root,
            "host",
            source.clone(),
            "Finish the implementation.",
            &Requirements {
                required_os: None,
                tools: vec!["python".into()],
            },
            &client,
        )
        .unwrap();
        assert_eq!(record.stage, "running");
        let calls = client.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .map(|v| v["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "agent.version",
                "host.status",
                "codex.handoffPreflight",
                "codex.handoffStart",
                "push",
                "codex.handoffStart"
            ]
        );
        let start = calls.last().unwrap();
        assert_eq!(start["effort"], "high");
        assert_eq!(start["model"], "model");
        assert_eq!(start["modelProvider"], "vellum");
        assert!(start["brief"]
            .as_str()
            .unwrap()
            .starts_with("Finish the implementation."));
        assert!(start["brief"]
            .as_str()
            .unwrap()
            .contains("use python3 instead"));
        assert_eq!(calls[3]["operationId"], start["operationId"]);
        assert_eq!(calls[4]["commit"], record.snapshot.commit);
        assert_eq!(
            load(&root, &record.id).unwrap().snapshot.commit,
            record.snapshot.commit
        );
    }

    #[test]
    fn lfs_preflight_returns_a_blocker_and_ref_notice_before_transport_or_snapshot() {
        let (_temp, root, source) = fixture();
        fs::write(source.repo.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        let report = local_preflight(&root, source.clone()).unwrap();
        assert!(report
            .blockers
            .iter()
            .any(|blocker| blocker.starts_with("HandoffLfsUnsupported")));
        assert!(report.local_ref_notice.contains("refs/vellum-handoff/<id>"));
        assert!(report.local_ref_notice.contains("discard"));
        let client = Fake::new();
        assert!(begin(
            &root,
            "host",
            source.clone(),
            "Continue.",
            &Requirements::default(),
            &client
        )
        .is_err());
        assert!(client.calls.borrow().is_empty());
        assert!(Git::new(&source.repo, &root)
            .run(&["for-each-ref", "refs/vellum-handoff/"], None)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn initial_local_journal_failure_releases_snapshot_before_any_remote_write() {
        let (_temp, root, source) = fixture();
        fs::write(&root, "not a directory").unwrap();
        let client = Fake::new();
        assert!(begin(
            &root,
            "host",
            source.clone(),
            "Continue.",
            &Requirements::default(),
            &client
        )
        .is_err());
        assert!(Git::new(&source.repo, &root)
            .run(&["for-each-ref", "refs/vellum-handoff/"], None)
            .unwrap()
            .is_empty());
        assert!(!client.calls.borrow().iter().any(|value| matches!(
            value["method"].as_str(),
            Some("push" | "codex.handoffStart")
        )));
    }
    #[test]
    fn platform_or_incomplete_runtime_refusal_never_prepares_pushes_or_creates_a_snapshot() {
        for incomplete in [false, true] {
            let (_temp, root, source) = fixture();
            let client = Fake::new();
            client.incomplete.set(incomplete);
            let requirements = Requirements {
                required_os: Some("windows".into()),
                tools: vec![],
            };
            let error = begin(
                &root,
                "host",
                source.clone(),
                "Build the task.",
                &requirements,
                &client,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(if incomplete {
                "codexRuntimeIncomplete"
            } else {
                "HandoffPlatformMismatch"
            }));
            assert!(!root.join("remote-handoffs/records").exists());
            assert!(!client
                .calls
                .borrow()
                .iter()
                .any(|v| matches!(v["method"].as_str(), Some("codex.handoffStart" | "push"))));
            assert!(Git::new(&source.repo, &root)
                .run(&["for-each-ref", "refs/vellum-handoff/"], None)
                .unwrap()
                .is_empty());
        }
    }
    #[test]
    fn ambiguous_start_is_observed_before_an_explicit_resume_and_keeps_the_same_snapshot() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        client.fail_start.set(true);
        assert!(begin(
            &root,
            "host",
            source.clone(),
            "Continue.",
            &Requirements::default(),
            &client
        )
        .is_err());
        let mut record = list_at(&root).unwrap().remove(0);
        assert_eq!(record.stage, "startUncertain");
        let commit = record.snapshot.commit.clone();
        fs::write(source.repo.join("file"), "local work after handoff\n").unwrap();
        let before = client.calls.borrow().len();
        assert!(resume_with(&root, &mut record, &client).is_err());
        assert_eq!(client.calls.borrow().len(), before + 2);
        assert_eq!(record.stage, "startUncertain");
        *client.result.borrow_mut() = json!({"state": "indeterminate", "phase": "threadAdopted", "threadId": "remote-thread"});
        client.fail_start.set(false);
        resume_with(&root, &mut record, &client).unwrap();
        let calls = client.calls.borrow();
        assert_eq!(calls[calls.len() - 2]["method"], "codex.handoffResult");
        assert_eq!(calls[calls.len() - 1]["method"], "codex.handoffStart");
        assert_eq!(calls[calls.len() - 1]["snapshotCommit"], commit);
        assert_eq!(calls.iter().filter(|v| v["method"] == "push").count(), 1);
        assert_eq!(
            fs::read_to_string(source.repo.join("file")).unwrap(),
            "local work after handoff\n"
        );
    }
    #[test]
    fn failed_push_can_resume_without_changing_the_prepared_identity() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        client.fail_push.set(true);
        assert!(begin(
            &root,
            "host",
            source,
            "Continue.",
            &Requirements::default(),
            &client
        )
        .is_err());
        let mut record = list_at(&root).unwrap().remove(0);
        assert_eq!(record.stage, "remotePrepared");
        let before = client.calls.borrow().len();
        poll_with(&root, &mut record, &client).unwrap();
        assert_eq!(record.stage, "remotePrepared");
        assert_eq!(client.calls.borrow().len(), before);
        client.fail_push.set(false);
        resume_with(&root, &mut record, &client).unwrap();
        assert_eq!(record.stage, "running");
        assert_eq!(
            client
                .calls
                .borrow()
                .iter()
                .filter(|v| v["method"] == "codex.handoffStart" && v["prepareOnly"] == true)
                .count(),
            1
        );
    }
    #[test]
    fn unsafe_prepared_paths_and_operation_lock_contention_are_rejected() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        let record = begin(
            &root,
            "host",
            source,
            "Continue.",
            &Requirements::default(),
            &client,
        )
        .unwrap();
        assert!(validate_prepared(&record, &json!({"repoPath": "/etc/passwd"})).is_err());
        let _guard = Lock::acquire(&root, &record.id).unwrap();
        assert!(Lock::acquire(&root, &record.id).is_err());
        assert!(path(&root, "../other").is_err());
    }

    #[test]
    fn polling_cannot_start_an_adopted_thread_or_accept_changed_identity() {
        let (_temp, root, source) = fixture();
        let client = Fake::new();
        let mut record = begin(
            &root,
            "host",
            source,
            "Continue.",
            &Requirements::default(),
            &client,
        )
        .unwrap();
        record.remote.as_mut().unwrap()["threadId"] = Value::Null;
        record.remote.as_mut().unwrap()["turnId"] = Value::Null;
        record.stage = "startUncertain".into();
        *client.result.borrow_mut() = json!({"state": "indeterminate", "phase": "threadAdopted", "threadId": "adopted-thread"});
        let before = client.calls.borrow().len();
        poll_with(&root, &mut record, &client).unwrap();
        assert_eq!(record.stage, "adopted");
        assert_eq!(client.calls.borrow().len(), before + 1);
        assert_eq!(
            client.calls.borrow().last().unwrap()["method"],
            "codex.handoffResult"
        );
        *client.result.borrow_mut() = json!({"state": "running", "phase": "running", "threadId": "different", "turnId": "remote-turn"});
        assert!(poll_with(&root, &mut record, &client).is_err());
        assert_eq!(record.stage, "adopted");
    }
}
