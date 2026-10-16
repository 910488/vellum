//! Local, bounded deployment evidence. Only closed-vocabulary data crosses here.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::operation::RemoteOperationProgress;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    Deploy,
    Redeploy,
    Models,
    Restore,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub persistence_scope: Option<String>,
}
impl Metadata {
    pub fn observe(&mut self, value: &Value) {
        for (field, pointers, allowed) in [
            (
                &mut self.os,
                ["/os", "/capabilities/os", "/bootstrap/os"],
                &["linux", "darwin", "macos", "windows"][..],
            ),
            (
                &mut self.arch,
                ["/arch", "/capabilities/arch", "/bootstrap/arch"],
                &["x86_64", "aarch64", "arm64", "amd64", "x86", "arm"][..],
            ),
            (
                &mut self.persistence_scope,
                [
                    "/persistenceScope",
                    "/capabilities/persistenceScope",
                    "/bootstrap/persistenceScope",
                ],
                &["linger", "login", "onDemand"][..],
            ),
        ] {
            if let Some(text) = pointers
                .iter()
                .find_map(|p| value.pointer(p).and_then(Value::as_str))
                .filter(|s| allowed.contains(s))
            {
                *field = Some(text.into());
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub phase: String,
    pub result: Outcome,
    pub ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub event: String,
    pub operation_id: String,
    pub time: DateTime<Utc>,
    pub build: String,
    #[serde(flatten)]
    pub metadata: Metadata,
    pub trigger: Trigger,
    pub phases: Vec<Phase>,
    pub result: Option<Outcome>,
    pub failure_code: Option<String>,
}
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub interrupted: usize,
    pub blocked: usize,
    pub last_failure_code: Option<String>,
}
fn error() -> AppError {
    AppError::Message("DeploymentLogUnavailable".into())
}
fn path(root: &Path, host: &str) -> AppResult<PathBuf> {
    if !super::handoff::valid_id(host) {
        return Err(AppError::Message("InvalidRemoteHostId".into()));
    }
    Ok(root.join("remote-deploy-log").join(format!("{host}.jsonl")))
}
// Same host granularity as converge_control; fs2 also serializes multiple app
// processes. Hold only for local I/O, never across an SSH call or async wait.
fn locked<T>(root: &Path, host: &str, work: impl FnOnce(&Path) -> AppResult<T>) -> AppResult<T> {
    let path = path(root, host)?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|_| error())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(|_| error())?;
    lock.lock_exclusive().map_err(|_| error())?;
    work(&path)
}
fn read(path: &Path) -> AppResult<Vec<Event>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(error()),
    };
    // A process crash can leave a partial last append. Ignore only that suffix;
    // corrupt complete lines are not silently accepted as trustworthy evidence.
    text.split_inclusive('\n')
        .filter(|s| s.ends_with('\n') && !s.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|_| error()))
        .collect()
}
fn encode(events: &[Event]) -> AppResult<Vec<u8>> {
    let mut bytes = vec![];
    for event in events {
        serde_json::to_writer(&mut bytes, event).map_err(|_| error())?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}
fn rewrite(path: &Path, events: &[Event]) -> AppResult<()> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&encode(events).map_err(|_| std::io::Error::other("encode"))?)?;
        file.sync_all()?;
        // std::fs::rename replaces the destination (MoveFileExW on Windows).
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result.map_err(|_| error())
}
fn append(path: &Path, event: &Event) -> AppResult<()> {
    let mut events = read(path)?;
    // Repair a torn suffix before appending, otherwise it would corrupt the next line.
    if let Ok(bytes) = fs::read(path) {
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            rewrite(path, &events)?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| error())?;
    file.write_all(&encode(std::slice::from_ref(event))?)
        .map_err(|_| error())?;
    file.sync_data().map_err(|_| error())?;
    drop(file);
    events.push(event.clone());
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e.event == "started")
        .map(|e| e.operation_id.clone())
        .collect();
    if starts.len() > 500 {
        let keep: BTreeSet<_> = starts.into_iter().rev().take(500).collect();
        events.retain(|e| keep.contains(&e.operation_id));
        rewrite(path, &events)?;
    }
    Ok(())
}
fn phase(value: &str) -> String {
    match value {
        "queued"
        | "hostPreflight"
        | "cleanHostPreflight"
        | "autoFix"
        | "installCodex"
        | "nativeDaemon"
        | "deploymentPlan"
        | "deploymentApply"
        | "verification"
        | "verificationRetry"
        | "observing"
        | "waitingForIdle"
        | "awaitingTakeover"
        | "verified"
        | "applying"
        | "restorePreflight"
        | "restoreConfig"
        | "stopProxy"
        | "restoreCodex"
        | "restoreSsh"
        | "restoreLease"
        | "restartNative"
        | "restoreVerification"
        | "restored"
        | "failed"
        | "cancelled" => value.into(),
        _ => "other".into(),
    }
}
// Never extract an arbitrary word from an error. Unknown errors get a fixed code.
fn failure_code(message: &str) -> String {
    const CODES: &[&str] = &[
        "psStartTimeUnavailable",
        "unsupportedRemotePlatform",
        "intelMacUnsupported",
        "insufficientDiskSpace",
        "systemdUserUnavailable",
        "proxyPortConflict",
        "nativeDaemonAppOwned",
        "incompleteObservation",
        "activeWorkInProgress",
        "officialAccountPairingRequired",
        "credentialMissing",
        "credentialUnavailable",
        "CodexDesktopProtocolMismatch",
        "RemoteBoundaryKeyProvisionFailed",
        "DeploymentControlUnavailable",
        "proxyMigrationRecoveryFailed",
        "proxyMigrationFailed",
        "proxyVerificationFailed",
        "nativeDaemonVerificationFailed",
        "detachedReadinessVerificationFailed",
        "SshHostKeyFingerprintMismatch",
        "DeploymentAlreadyRunning",
        "activeTurnInProgress",
        "OneClickRestoreConflict",
        "OneClickRestoreVerificationFailed",
        "DeploymentAgentUpgradeRequired",
        "DeploymentConfirmationExpired",
        "bundledReleaseUnavailable",
        "codexRuntimeIncomplete",
        "agentUnavailable",
        "codexRestartRequired",
        "credentialsMissing",
        "desktopOfficialAccountMissing",
        "dockerUnavailable",
        "guiSessionUnavailable",
        "proxyPortOccupied",
        "proxyMigrationRolledBack",
        "proxyMigrationAgentUpgradeRequired",
        "deploymentVerificationFailed",
        "hostNotConfigured",
        "injectionRequiresReadyProxy",
        "invalidCompactionThreshold",
        "managedRuntimeRecoveryRequired",
        "nativeCodexVersionMismatch",
        "noModelsSelected",
        "noDeployableModels",
        "officialAccountActivationRequired",
        "officialAccountReauthenticationRequired",
        "officialAccountStatusUnavailable",
        "proxyConfigurationMissing",
        "proxyConfigurationSchemaTooNew",
        "proxyConfigurationUnreadable",
        "versionMismatch",
        "windowsOpenSshUnavailable",
        "NativeProxyRuntimeNotReady",
        "NativeProxyServiceExited",
    ];
    message
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .find(|word| CODES.contains(word))
        .unwrap_or("DeploymentFailed")
        .into()
}
fn outcome(progress: &RemoteOperationProgress) -> Outcome {
    match progress.state.as_str() {
        "completed" => Outcome::Succeeded,
        "cancelled" => Outcome::Cancelled,
        _ if progress.message.as_deref().is_some_and(|m| {
            m.starts_with("OneClickBootstrapBlocked:")
                || m.starts_with("BootstrapPreflightBlocked:")
                || m.starts_with("OneClickRestoreBlocked:")
        }) =>
        {
            Outcome::Blocked
        }
        _ => Outcome::Failed,
    }
}
pub(crate) struct Recorder {
    root: PathBuf,
    host: String,
    event: Event,
    current: String,
    tick: Instant,
}
impl Recorder {
    pub fn begin(
        root: &Path,
        progress: &RemoteOperationProgress,
        trigger: Trigger,
        snapshot: Option<Value>,
    ) -> AppResult<Self> {
        if !super::handoff::valid_id(&progress.operation_id) {
            return Err(error());
        }
        let mut metadata = Metadata::default();
        if let Some(value) = snapshot {
            metadata.observe(value.get("status").unwrap_or(&value));
        }
        let event = Event {
            event: "started".into(),
            operation_id: progress.operation_id.clone(),
            time: Utc::now(),
            build: env!("CARGO_PKG_VERSION").into(),
            metadata,
            trigger,
            phases: vec![],
            result: None,
            failure_code: None,
        };
        locked(root, &progress.host_id, |path| append(path, &event))?;
        Ok(Self {
            root: root.into(),
            host: progress.host_id.clone(),
            event,
            current: phase(&progress.phase),
            tick: Instant::now(),
        })
    }
    pub fn phase(&mut self, next: &str) {
        let next = phase(next);
        if next != self.current {
            self.close_phase(if next == "verificationRetry" {
                Outcome::Failed
            } else {
                Outcome::Succeeded
            });
            self.current = next;
        }
    }
    fn close_phase(&mut self, result: Outcome) {
        self.event.phases.push(Phase {
            phase: self.current.clone(),
            result,
            ms: self.tick.elapsed().as_millis().min(u64::MAX as u128) as u64,
        });
        self.tick = Instant::now();
    }
    pub fn finish(mut self, progress: &RemoteOperationProgress) -> AppResult<()> {
        let result = outcome(progress);
        self.close_phase(result.clone());
        if let Some(value) = &progress.result {
            self.event.metadata.observe(value);
        }
        self.event.event = "finished".into();
        self.event.time = Utc::now();
        self.event.failure_code = matches!(result, Outcome::Failed | Outcome::Blocked)
            .then(|| failure_code(progress.message.as_deref().unwrap_or("")));
        self.event.result = Some(result);
        locked(&self.root, &self.host, |path| append(path, &self.event))
    }
}
pub fn history(root: &Path, host: &str) -> AppResult<Vec<Event>> {
    locked(root, host, |path| {
        let mut events = read(path)?;
        let finished: BTreeSet<_> = events
            .iter()
            .filter(|e| e.event == "finished")
            .map(|e| e.operation_id.clone())
            .collect();
        let orphaned: Vec<_> = events
            .iter()
            .filter(|e| e.event == "started" && !finished.contains(&e.operation_id))
            .cloned()
            .collect();
        for mut event in orphaned {
            if super::converge_control::resolve(&event.operation_id, host).is_ok() {
                continue;
            }
            // Reconcile stale running operation files after app restart too.
            let progress = super::operation::load_reconciled(root, &event.operation_id).ok();
            if progress.as_ref().is_some_and(|p| p.state == "running") {
                continue;
            }
            event.event = "finished".into();
            event.time = Utc::now();
            event.result = Some(Outcome::Interrupted);
            event.failure_code = Some("DeploymentInterrupted".into());
            if let Some(p) = progress {
                event.phases.push(Phase {
                    phase: phase(&p.phase),
                    result: Outcome::Interrupted,
                    ms: 0,
                });
            }
            append(path, &event)?;
            events.push(event);
        }
        read(path)
    })
}
// Order attempts by their started event, not by recovery time: repairing an old
// orphan must not replace a more recent successful attempt in the UI.
fn completed(events: &[Event]) -> impl DoubleEndedIterator<Item = &Event> {
    let finished: BTreeMap<_, _> = events
        .iter()
        .filter(|e| e.event == "finished")
        .map(|e| (e.operation_id.as_str(), e))
        .collect();
    events
        .iter()
        .filter(|e| e.event == "started")
        .filter_map(move |e| finished.get(e.operation_id.as_str()).copied())
}
pub fn summary(root: &Path, host: &str) -> AppResult<Summary> {
    let mut summary = Summary::default();
    for event in completed(&history(root, host)?) {
        match event.result {
            Some(Outcome::Succeeded) => summary.succeeded += 1,
            Some(Outcome::Failed) => summary.failed += 1,
            Some(Outcome::Interrupted) => summary.interrupted += 1,
            Some(Outcome::Blocked) => summary.blocked += 1,
            _ => continue,
        }
        summary.last_failure_code = if matches!(
            event.result,
            Some(Outcome::Failed | Outcome::Blocked | Outcome::Interrupted)
        ) {
            event.failure_code.clone()
        } else {
            None
        };
    }
    summary.total = summary.succeeded + summary.failed + summary.interrupted;
    Ok(summary)
}
pub fn recent(root: &Path, host: &str) -> AppResult<Vec<Event>> {
    let events = history(root, host)?;
    let ids: BTreeSet<_> = completed(&events)
        .rev()
        .take(20)
        .map(|e| &e.operation_id)
        .collect();
    Ok(events
        .iter()
        .filter(|e| ids.contains(&e.operation_id))
        .cloned()
        .collect())
}

/// Offline evidence contains no operation messages, results, consent tokens or
/// SSH destinations. The remote half keeps the agent's existing redaction.
pub(crate) fn support_bundle(
    root: &Path,
    host: &str,
    fetch: impl FnOnce() -> AppResult<Value>,
) -> AppResult<Value> {
    let events = recent(root, host)?;
    let operations: Vec<_> = super::operation::for_host(root, host)?
        .into_iter()
        .map(|p| {
            serde_json::json!({"operationId": p.operation_id, "kind": p.kind, "state": p.state,
            "phase": phase(&p.phase), "updatedAt": p.updated_at})
        })
        .collect();
    let remote = fetch();
    let value = serde_json::json!({"deploymentLog": events, "operations": operations,
        "remoteUnavailable": remote.is_err(), "remote": remote.ok()});
    let path = root
        .join("remote-support-bundles")
        .join(format!("{}.json", ulid::Ulid::new()));
    crate::enhanced_runtime::atomic::write_atomic(
        &path,
        &serde_json::to_vec_pretty(&value).map_err(|_| error())?,
    )
    .map_err(|_| error())?;
    Ok(
        serde_json::json!({"path": path, "bundle": value, "remoteUnavailable": value["remoteUnavailable"]}),
    )
}

#[cfg(test)]
#[path = "deploy_log_tests.rs"]
mod tests;
