//! Production remote apply: signed package over SSH, host idle observation,
//! and a host-side helper that is not the agent process.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::apply::IdleEvidence;
use super::remote_helper::RemoteBackend;
use crate::error::AppResult;
use crate::process::background_command;
use crate::remote::agent_client::RemoteAgentClient;
use crate::remote::ssh_trust;
use crate::state::AppState;

pub const HOST_HELPER_SH: &str = include_str!("host_helper.sh");

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostFacts {
    pub observed: bool,
    pub proxy_in_flight: u64,
    pub native_active_turns: u32,
    pub tools_running: u32,
    pub pending_approvals: u32,
    pub heartbeat_expired: bool,
    pub unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidationReport {
    pub agent_version: Option<String>,
    pub broker_version: Option<String>,
    pub proxy_image: Option<String>,
    pub agent_protocol: Option<u64>,
    pub task_list: bool,
    pub events_ok: bool,
    pub health_only: bool,
}

/// Identity of the running agent+broker+proxy set. Apply succeeds only when
/// this equals the staged signed `remote-v*` package, not when fields exist.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningSetIdentity {
    pub agent_version: String,
    pub broker_version: String,
    pub proxy_image: String,
}

impl RunningSetIdentity {
    pub fn from_remote_version(version: &str) -> Self {
        Self {
            agent_version: version.to_string(),
            broker_version: version.to_string(),
            proxy_image: format!("vellum-proxy:{version}"),
        }
    }

    pub fn from_report(report: &ValidationReport) -> Option<Self> {
        Some(Self {
            agent_version: report
                .agent_version
                .clone()
                .filter(|value| !value.is_empty())?,
            broker_version: report
                .broker_version
                .clone()
                .filter(|value| !value.is_empty())?,
            proxy_image: report
                .proxy_image
                .clone()
                .filter(|value| !value.is_empty())?,
        })
    }
}

pub fn expected_identity_from_staged(path: &Path, target_version: &str) -> RunningSetIdentity {
    if let Ok(bytes) = std::fs::read(path) {
        if let Some(identity) = identity_from_tar_gz(&bytes) {
            return identity;
        }
        if let Some(identity) = identity_from_json(&bytes) {
            return identity;
        }
    }
    let sidecar = path.with_extension("identity.json");
    if let Ok(bytes) = std::fs::read(&sidecar) {
        if let Some(identity) = identity_from_json(&bytes) {
            return identity;
        }
    }
    RunningSetIdentity::from_remote_version(target_version)
}

fn identity_from_tar_gz(bytes: &[u8]) -> Option<RunningSetIdentity> {
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().ok()?;
    for entry in entries {
        let mut entry = entry.ok()?;
        let path = entry.path().ok()?;
        let name = path.file_name()?.to_string_lossy().into_owned();
        if name == "manifest.json" || name == "identity.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).ok()?;
            return identity_from_json(&buf);
        }
    }
    None
}

fn identity_from_json(bytes: &[u8]) -> Option<RunningSetIdentity> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let agent = pointer_string(&value, "/agent/version")
        .or_else(|| pointer_string(&value, "/agentVersion"))?;
    let broker = pointer_string(&value, "/broker/version")
        .or_else(|| pointer_string(&value, "/brokerVersion"))?;
    let proxy =
        pointer_string(&value, "/proxy/image").or_else(|| pointer_string(&value, "/proxyImage"))?;
    if agent.is_empty() || broker.is_empty() || proxy.is_empty() {
        return None;
    }
    Some(RunningSetIdentity {
        agent_version: agent,
        broker_version: broker,
        proxy_image: proxy,
    })
}

pub fn idle_from_host_facts(facts: &HostFacts) -> IdleEvidence {
    IdleEvidence {
        proxy_idle: facts.observed && facts.proxy_in_flight == 0 && !facts.unknown,
        native_sessions_idle: facts.observed && facts.native_active_turns == 0 && !facts.unknown,
        tools_idle: facts.observed && facts.tools_running == 0 && !facts.unknown,
        approvals_idle: facts.observed && facts.pending_approvals == 0 && !facts.unknown,
        heartbeat_expired: facts.heartbeat_expired,
        observation_fresh: facts.observed && !facts.heartbeat_expired && !facts.unknown,
        unknown_work: !facts.observed || facts.unknown,
        open_turns: facts.native_active_turns,
        durable_handoff_ready: None,
    }
}

pub fn host_facts_from_json(
    status: &Value,
    session: Option<&Value>,
    proxy: Option<&Value>,
) -> HostFacts {
    let proxy_in_flight = proxy
        .and_then(|value| {
            value
                .pointer("/inFlight")
                .or_else(|| value.pointer("/activeRequests"))
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            status
                .pointer("/proxy/inFlight")
                .or_else(|| status.pointer("/proxy/activeRequests"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let mut native_active_turns = 0u32;
    let mut tools_running = 0u32;
    let mut pending_approvals = status
        .pointer("/pendingApprovals")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let mut unknown = false;
    if let Some(session) = session {
        if let Some(threads) = session
            .pointer("/threads")
            .or_else(|| session.get("threads"))
            .and_then(Value::as_array)
        {
            for thread in threads {
                let active = thread
                    .get("active")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| {
                        thread.pointer("/status").and_then(Value::as_str) == Some("active")
                    });
                if active || thread.get("activeTurnId").and_then(Value::as_str).is_some() {
                    native_active_turns = native_active_turns.saturating_add(1);
                }
                if thread
                    .pointer("/toolRunning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    tools_running = tools_running.saturating_add(1);
                }
                pending_approvals = pending_approvals.saturating_add(
                    thread
                        .pointer("/pendingApprovals")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32,
                );
            }
        } else {
            unknown = true;
        }
    } else {
        unknown = true;
    }
    HostFacts {
        observed: true,
        proxy_in_flight,
        native_active_turns,
        tools_running,
        pending_approvals,
        heartbeat_expired: status
            .pointer("/heartbeatExpired")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        unknown,
    }
}

pub fn require_full_validation(
    report: &ValidationReport,
    expected: &RunningSetIdentity,
) -> Result<(), String> {
    if report.health_only {
        return Err("refusing /health-only validation".into());
    }
    if report.agent_protocol.is_none() {
        return Err("handshake missing agentProtocol".into());
    }
    if !report.task_list {
        return Err("task list not observed".into());
    }
    if !report.events_ok {
        return Err("event channel not observed".into());
    }
    let Some(observed) = RunningSetIdentity::from_report(report) else {
        return Err("inventory missing agentVersion/brokerVersion/proxy image".into());
    };
    if observed != *expected {
        return Err(format!(
            "running set identity does not match staged remote-v* package: observed agent={} broker={} proxy={}; expected agent={} broker={} proxy={}",
            observed.agent_version,
            observed.broker_version,
            observed.proxy_image,
            expected.agent_version,
            expected.broker_version,
            expected.proxy_image
        ));
    }
    Ok(())
}

pub fn validation_from_json(
    inventory: &Value,
    handshake: &Value,
    snapshot: &Value,
) -> ValidationReport {
    let health_only = inventory.get("status").and_then(Value::as_str) == Some("ok")
        && inventory.get("agentVersion").is_none()
        && inventory.pointer("/agentVersion").is_none();
    ValidationReport {
        agent_version: pointer_string(inventory, "/agentVersion")
            .or_else(|| pointer_string(inventory, "/agent/version")),
        broker_version: pointer_string(inventory, "/brokerVersion")
            .or_else(|| pointer_string(inventory, "/broker/version")),
        proxy_image: pointer_string(inventory, "/proxy/image")
            .or_else(|| pointer_string(inventory, "/proxyImage")),
        agent_protocol: handshake
            .get("agentProtocol")
            .and_then(Value::as_u64)
            .or_else(|| inventory.get("agentProtocol").and_then(Value::as_u64)),
        task_list: snapshot.get("threads").and_then(Value::as_array).is_some()
            || snapshot
                .pointer("/threads")
                .and_then(Value::as_array)
                .is_some()
            || snapshot
                .pointer("/tasks")
                .and_then(Value::as_array)
                .is_some(),
        events_ok: snapshot.pointer("/events").is_some()
            || snapshot
                .pointer("/eventChannel")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || snapshot
                .pointer("/proxy/eventsReady")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        health_only,
    }
}

fn pointer_string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub trait RemoteHostOps: Send {
    fn try_host_lock(&mut self, host_id: &str, operation_id: &str) -> Result<bool, String>;
    fn observe_host(&self) -> Result<HostFacts, String>;
    fn upload_package(&mut self, sha256: &str, bytes: &[u8]) -> Result<(), String>;
    fn start_helper(&mut self, action: &str) -> Result<(), String>;
    fn read_host_steps(&self) -> Vec<String>;
    fn validate_host(&self) -> Result<ValidationReport, String>;
}

pub struct AppRemoteBackend<O: RemoteHostOps> {
    pub ops: O,
    pub staged_local: PathBuf,
    pub package_sha256: String,
    pub uploaded_sha256: Option<String>,
    pub expected: RunningSetIdentity,
}

impl<O: RemoteHostOps> RemoteBackend for AppRemoteBackend<O> {
    fn try_lock(&mut self, host_id: &str, operation_id: &str) -> Result<bool, String> {
        self.ops.try_host_lock(host_id, operation_id)
    }

    fn observe(&self) -> IdleEvidence {
        match self.ops.observe_host() {
            Ok(facts) => idle_from_host_facts(&facts),
            Err(_) => IdleEvidence {
                unknown_work: true,
                observation_fresh: false,
                ..IdleEvidence::default()
            },
        }
    }

    fn stage_package(&mut self, sha256: &str) -> Result<(), String> {
        if sha256.is_empty() {
            return Err("missing package sha256".into());
        }
        let bytes = std::fs::read(&self.staged_local).map_err(|error| {
            format!(
                "read signed remote package {}: {error}",
                self.staged_local.display()
            )
        })?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(sha256)
            && !actual.eq_ignore_ascii_case(&self.package_sha256)
        {
            return Err("staged remote package does not match signed sha256".into());
        }
        self.ops.upload_package(sha256, &bytes)?;
        self.uploaded_sha256 = Some(sha256.to_string());
        Ok(())
    }

    fn replace_set(&mut self) -> Result<(), String> {
        let Some(sha) = &self.uploaded_sha256 else {
            return Err("replace_set without staged signed package".into());
        };
        if sha != &self.package_sha256 && !self.package_sha256.is_empty() {
            return Err("refusing bundled pin; signed remote-v* package was not staged".into());
        }
        self.ops.start_helper("apply")
    }

    fn validate_full(&self) -> Result<(), String> {
        require_full_validation(&self.ops.validate_host()?, &self.expected)
    }

    fn rollback_set(&mut self) -> Result<(), String> {
        self.ops.start_helper("rollback")
    }

    fn host_journal_steps(&self) -> Vec<String> {
        self.ops.read_host_steps()
    }
}

pub struct SshRemoteHostOps {
    client: RemoteAgentClient,
    destination: String,
    remote_root: String,
    pub expected: RunningSetIdentity,
}

impl SshRemoteHostOps {
    pub fn from_state(state: &AppState, host_id: &str, operation_id: &str) -> AppResult<Self> {
        let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
        let destination = target.ssh_destination.clone().ok_or_else(|| {
            crate::error::AppError::Message("remote update requires an SSH destination".into())
        })?;
        Ok(Self {
            client: RemoteAgentClient::new(target),
            destination,
            remote_root: format!("/tmp/vellum-update/{operation_id}"),
            expected: RunningSetIdentity::default(),
        })
    }

    fn ssh_sh(&self, script: &str) -> Result<String, String> {
        ssh_sh(&self.destination, script)
    }
}

impl RemoteHostOps for SshRemoteHostOps {
    fn try_host_lock(&mut self, _host_id: &str, operation_id: &str) -> Result<bool, String> {
        let script = format!(
            "mkdir -p {root} && \
             if [ ! -f {root}/lock ]; then printf '%s' '{op}' > {root}/lock; fi && \
             cat {root}/lock",
            root = self.remote_root,
            op = operation_id,
        );
        let held = self.ssh_sh(&script)?;
        Ok(held.trim() == operation_id)
    }

    fn observe_host(&self) -> Result<HostFacts, String> {
        let status = self
            .client
            .host_status()
            .map_err(|error| error.to_string())?;
        let session = self.client.codex_session_status(None).ok();
        let proxy = self.client.proxy_status().ok();
        Ok(host_facts_from_json(
            &status,
            session.as_ref(),
            proxy.as_ref(),
        ))
    }

    fn upload_package(&mut self, sha256: &str, bytes: &[u8]) -> Result<(), String> {
        let package = format!("{}/package.tar.gz", self.remote_root);
        let helper = format!("{}/helper.sh", self.remote_root);
        self.ssh_sh(&format!("mkdir -p {}", self.remote_root))?;
        self.client
            .stage_artifact(&package, bytes)
            .map_err(|error| error.to_string())?;
        self.client
            .stage_artifact(&helper, HOST_HELPER_SH.as_bytes())
            .map_err(|error| error.to_string())?;
        let expected_json =
            serde_json::to_vec(&self.expected).map_err(|error| error.to_string())?;
        let expected_path = format!("{}/expected.json", self.remote_root);
        self.client
            .stage_artifact(&expected_path, &expected_json)
            .map_err(|error| error.to_string())?;
        self.ssh_sh(&format!(
            "chmod +x {helper} && printf '%s' '{sha}' > {root}/sha256",
            helper = helper,
            sha = sha256,
            root = self.remote_root,
        ))?;
        Ok(())
    }

    fn start_helper(&mut self, action: &str) -> Result<(), String> {
        // nohup so Vellum close / SSH drop cannot abort the helper.
        self.ssh_sh(&format!(
            "nohup sh {root}/helper.sh {root} {action} >{root}/helper.log 2>&1 & echo $!",
            root = self.remote_root,
            action = action,
        ))?;
        // Best-effort wait for a journal step; resume uses host_journal_steps.
        let _ = self.ssh_sh(&format!(
            "for i in 1 2 3 4 5 6 7 8 9 10; do \
               if grep -qE 'commit|rollback|replaceSet' {root}/steps 2>/dev/null; then cat {root}/steps; exit 0; fi; \
               sleep 1; \
             done; cat {root}/steps 2>/dev/null || true",
            root = self.remote_root,
        ));
        Ok(())
    }

    fn read_host_steps(&self) -> Vec<String> {
        let Ok(body) = self.ssh_sh(&format!(
            "cat {}/steps 2>/dev/null || true",
            self.remote_root
        )) else {
            return Vec::new();
        };
        body.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn validate_host(&self) -> Result<ValidationReport, String> {
        let inventory = self
            .client
            .host_inventory_v2()
            .map_err(|error| error.to_string())?;
        let handshake = self
            .client
            .agent_version()
            .map_err(|error| error.to_string())?;
        let snapshot = self
            .client
            .host_manager_snapshot(None, None)
            .or_else(|_| self.client.codex_session_status(None))
            .map_err(|error| error.to_string())?;
        Ok(validation_from_json(&inventory, &handshake, &snapshot))
    }
}

pub fn observe_remote_host(state: &AppState, host_id: &str) -> IdleEvidence {
    match SshRemoteHostOps::from_state(state, host_id, "observe") {
        Ok(ops) => match ops.observe_host() {
            Ok(facts) => idle_from_host_facts(&facts),
            Err(_) => IdleEvidence {
                unknown_work: true,
                observation_fresh: false,
                ..IdleEvidence::default()
            },
        },
        Err(_) => IdleEvidence {
            unknown_work: true,
            observation_fresh: false,
            ..IdleEvidence::default()
        },
    }
}

fn ssh_sh(destination: &str, script: &str) -> Result<String, String> {
    let data_root = ssh_trust::runtime_data_root();
    let trust_target =
        ssh_trust::resolve_ssh_target(destination).map_err(|error| error.to_string())?;
    ssh_trust::require_trust_or_error(&data_root, &trust_target)
        .map_err(|error| error.to_string())?;
    let mut cmd = background_command("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .args(ssh_trust::strict_host_key_args(&data_root))
        .arg("-T")
        .arg(destination)
        .arg("sh")
        .arg("-s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("ssh helper spawn failed: {error}"))?;
    {
        use std::io::Write;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "ssh helper stdin unavailable".to_string())?;
        stdin
            .write_all(script.as_bytes())
            .map_err(|error| format!("ssh helper stdin: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("ssh helper wait: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::super::apply::ApplyDecision;
    use super::super::journal::Journal;
    use super::super::remote_helper::{apply_remote_package, RemoteApplyPlan};
    use super::*;

    #[derive(Default)]
    struct RecordingHostOps {
        lock_holder: Option<String>,
        facts: HostFacts,
        uploaded: Option<(String, usize)>,
        helper_actions: Vec<String>,
        steps: Vec<String>,
        report: ValidationReport,
        fail_validate: bool,
    }

    impl RemoteHostOps for RecordingHostOps {
        fn try_host_lock(&mut self, _host_id: &str, operation_id: &str) -> Result<bool, String> {
            match &self.lock_holder {
                None => {
                    self.lock_holder = Some(operation_id.into());
                    Ok(true)
                }
                Some(held) => Ok(held == operation_id),
            }
        }

        fn observe_host(&self) -> Result<HostFacts, String> {
            Ok(self.facts.clone())
        }

        fn upload_package(&mut self, sha256: &str, bytes: &[u8]) -> Result<(), String> {
            self.uploaded = Some((sha256.to_string(), bytes.len()));
            self.steps.push("stage".into());
            Ok(())
        }

        fn start_helper(&mut self, action: &str) -> Result<(), String> {
            if action == "apply" && self.uploaded.is_none() {
                return Err("helper apply without signed package".into());
            }
            self.helper_actions.push(action.into());
            if action == "apply" {
                self.steps.push("replaceSet".into());
                self.steps.push("commit".into());
            }
            if action == "rollback" {
                self.steps.push("rollback".into());
            }
            Ok(())
        }

        fn read_host_steps(&self) -> Vec<String> {
            self.steps.clone()
        }

        fn validate_host(&self) -> Result<ValidationReport, String> {
            if self.fail_validate {
                return Ok(ValidationReport {
                    health_only: true,
                    ..ValidationReport::default()
                });
            }
            Ok(self.report.clone())
        }
    }

    fn idle_facts() -> HostFacts {
        HostFacts {
            observed: true,
            ..HostFacts::default()
        }
    }

    fn full_report() -> ValidationReport {
        ValidationReport {
            agent_version: Some("0.4.0".into()),
            broker_version: Some("0.4.0".into()),
            proxy_image: Some("vellum-proxy:0.4.0".into()),
            agent_protocol: Some(3),
            task_list: true,
            events_ok: true,
            health_only: false,
        }
    }

    fn sha_of(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn plan(sha: &str) -> RemoteApplyPlan {
        RemoteApplyPlan {
            operation_id: "op-host-1".into(),
            host_id: "host-a".into(),
            target_version: "0.4.0".into(),
            package_sha256: sha.into(),
        }
    }

    #[test]
    fn busy_host_turns_are_not_local_appstate_idle() {
        let facts = host_facts_from_json(
            &serde_json::json!({"proxy":{"inFlight":0}}),
            Some(&serde_json::json!({
                "threads": [{"id":"t1","status":"active","activeTurnId":"turn-1"}]
            })),
            Some(&serde_json::json!({"inFlight":0})),
        );
        let evidence = idle_from_host_facts(&facts);
        assert_eq!(evidence.open_turns, 1);
        assert!(!evidence.native_sessions_idle);
        assert!(evidence.observation_fresh);
    }

    #[test]
    fn missing_session_is_unknown_work_not_idle() {
        let facts = host_facts_from_json(&serde_json::json!({}), None, None);
        let evidence = idle_from_host_facts(&facts);
        assert!(evidence.unknown_work);
        assert!(!evidence.observation_fresh);
    }

    #[test]
    fn health_only_inventory_is_rejected() {
        let report = validation_from_json(
            &serde_json::json!({"status":"ok"}),
            &serde_json::json!({}),
            &serde_json::json!({}),
        );
        let err =
            require_full_validation(&report, &RunningSetIdentity::from_remote_version("0.4.0"))
                .unwrap_err();
        assert!(err.contains("/health-only"));
    }

    #[test]
    fn app_remote_backend_uses_signed_package_and_host_idle() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("vellum-remote-linux-x64.tar.gz");
        let payload = b"signed-remote-package";
        std::fs::write(&staged, payload).unwrap();
        let sha = sha_of(payload);
        let ops = RecordingHostOps {
            facts: HostFacts {
                observed: true,
                native_active_turns: 2,
                ..HostFacts::default()
            },
            report: full_report(),
            ..RecordingHostOps::default()
        };
        let mut backend = AppRemoteBackend {
            ops,
            staged_local: staged.clone(),
            package_sha256: sha.clone(),
            uploaded_sha256: None,
            expected: RunningSetIdentity::from_remote_version("0.4.0"),
        };
        let mut journal = Journal::default();
        let outcome =
            apply_remote_package(dir.path(), &mut journal, &mut backend, &plan(&sha), true)
                .unwrap();
        assert!(matches!(outcome.decision, ApplyDecision::Wait { .. }));
        assert!(backend.ops.uploaded.is_none() || backend.ops.helper_actions.is_empty());
        assert!(!backend.ops.helper_actions.iter().any(|a| a == "apply"));

        backend.ops.facts = idle_facts();
        let outcome =
            apply_remote_package(dir.path(), &mut journal, &mut backend, &plan(&sha), true)
                .unwrap();
        assert_eq!(outcome.decision, ApplyDecision::Allow);
        assert_eq!(
            backend.ops.uploaded.as_ref().map(|item| item.1),
            Some(b"signed-remote-package".len())
        );
        assert_eq!(backend.ops.helper_actions, vec!["apply".to_string()]);
        assert!(journal.already_applied("op-host-1"));
    }

    #[test]
    fn vellum_close_resume_reads_host_journal_not_bundled_pin() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("pkg.tar.gz");
        std::fs::write(&staged, b"pkg").unwrap();
        let sha = sha_of(b"pkg");
        let mut backend = AppRemoteBackend {
            ops: RecordingHostOps {
                facts: idle_facts(),
                report: full_report(),
                steps: vec!["lock".into(), "stage".into(), "commit".into()],
                uploaded: Some((sha.clone(), 3)),
                ..RecordingHostOps::default()
            },
            staged_local: staged,
            package_sha256: sha.clone(),
            uploaded_sha256: Some(sha.clone()),
            expected: RunningSetIdentity::from_remote_version("0.4.0"),
        };
        let mut journal = Journal::default();
        let outcome =
            apply_remote_package(dir.path(), &mut journal, &mut backend, &plan(&sha), true)
                .unwrap();
        assert_eq!(outcome.steps, vec!["alreadyApplied".to_string()]);
        assert!(!backend.ops.helper_actions.iter().any(|a| a == "apply"));
    }

    #[test]
    fn validate_health_only_rolls_back_helper() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("pkg.tar.gz");
        std::fs::write(&staged, b"pkg").unwrap();
        let sha = sha_of(b"pkg");
        let mut backend = AppRemoteBackend {
            ops: RecordingHostOps {
                facts: idle_facts(),
                fail_validate: true,
                ..RecordingHostOps::default()
            },
            staged_local: staged,
            package_sha256: sha.clone(),
            uploaded_sha256: None,
            expected: RunningSetIdentity::from_remote_version("0.4.0"),
        };
        let mut journal = Journal::default();
        let err = apply_remote_package(dir.path(), &mut journal, &mut backend, &plan(&sha), true)
            .unwrap_err();
        assert!(err.contains("/health-only"));
        assert!(backend.ops.helper_actions.iter().any(|a| a == "rollback"));
    }

    #[test]
    fn host_helper_script_verifies_hash_guards_tar_and_restarts_the_set() {
        let script = HOST_HELPER_SH;
        assert!(
            script.contains("package sha256 mismatch") && script.contains("sha256sum"),
            "helper must verify package.tar.gz against $ROOT/sha256"
        );
        assert!(
            script.contains("illegal tar member") && script.contains("*..*"),
            "helper must reject zip-slip / absolute tar members"
        );
        assert!(
            script.contains("restart_running_set")
                && script.contains("docker stop")
                && script.contains("docker rm")
                && script.contains("docker run")
                && (script.contains("pkill") || script.contains("systemctl")),
            "helper must stop/restart agent, broker, and proxy; docker load is not a switch"
        );
        assert!(
            script.contains("running.json"),
            "helper must record the new running identity"
        );
        let mutations_without_restart = script
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                (trimmed.contains("cp ") || trimmed.contains("docker load"))
                    && !trimmed.starts_with('#')
            })
            .count();
        assert!(
            mutations_without_restart > 0 && script.contains("restart_running_set"),
            "install copies are allowed only if the live set is restarted afterwards"
        );
        assert!(
            script.contains("tar-members.txt") && !script.contains("tar -tzf \"$PKG\" | while"),
            "zip-slip guard must not pipe tar into while (exit 1 would only kill the subshell)"
        );
    }

    fn write_zip_slip_targz(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        let name = b"../escape.txt";
        {
            let gnu = header.as_gnu_mut().expect("gnu header");
            gnu.name[..name.len()].copy_from_slice(name);
            gnu.name[name.len()] = 0;
        }
        let payload = b"pwned";
        header.set_size(payload.len() as u64);
        header.set_cksum();
        builder.append(&header, &payload[..]).unwrap();
        builder.finish().unwrap();
    }

    fn unix_shell() -> PathBuf {
        let git = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        if git.is_file() {
            return git;
        }
        PathBuf::from("bash")
    }

    fn to_msys_path(path: &Path) -> String {
        let raw = path.to_string_lossy();
        let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
        let bytes = stripped.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = stripped[2..].replace('\\', "/");
            format!("/{drive}{rest}")
        } else {
            stripped.replace('\\', "/")
        }
    }

    #[test]
    fn zip_slip_package_fails_before_extract() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("op");
        std::fs::create_dir_all(root.join("stage")).unwrap();
        let pkg = root.join("package.tar.gz");
        write_zip_slip_targz(&pkg);
        let digest = hex::encode(Sha256::digest(std::fs::read(&pkg).unwrap()));
        std::fs::write(root.join("sha256"), digest.as_bytes()).unwrap();
        let helper = dir.path().join("host_helper.sh");
        std::fs::write(&helper, HOST_HELPER_SH.replace("\r\n", "\n")).unwrap();

        let output = std::process::Command::new(unix_shell())
            .arg(to_msys_path(&helper))
            .arg(to_msys_path(&root))
            .arg("apply")
            .output()
            .expect("execute shipped host_helper.sh");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success(),
            "zip-slip tar must fail the helper before extract; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stderr.contains("illegal tar member") || stdout.contains("illegal tar member"),
            "helper must name the illegal member; stderr={stderr} stdout={stdout}"
        );
        assert!(
            !dir.path().join("escape.txt").exists(),
            "zip-slip member ../escape.txt must not be extracted outside STAGE"
        );
        assert!(
            std::fs::read_dir(root.join("stage"))
                .unwrap()
                .next()
                .is_none(),
            "STAGE must stay empty when the package is rejected"
        );
        let steps = std::fs::read_to_string(root.join("steps")).unwrap_or_default();
        assert!(
            !steps.lines().any(|line| line.trim() == "stage"),
            "stage must not be journaled after a zip-slip reject: {steps}"
        );
    }

    #[test]
    fn old_running_identity_after_helper_commit_is_not_applied() {
        let expected = RunningSetIdentity::from_remote_version("0.4.0");
        let old = ValidationReport {
            agent_version: Some("0.3.0".into()),
            broker_version: Some("0.3.0".into()),
            proxy_image: Some("vellum-proxy:0.3.0".into()),
            agent_protocol: Some(3),
            task_list: true,
            events_ok: true,
            health_only: false,
        };
        let err = require_full_validation(&old, &expected).unwrap_err();
        assert!(
            err.contains("does not match staged remote-v*"),
            "presence of inventory fields is not success: {err}"
        );

        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("pkg.tar.gz");
        std::fs::write(&staged, b"pkg").unwrap();
        let sha = sha_of(b"pkg");
        let mut backend = AppRemoteBackend {
            ops: RecordingHostOps {
                facts: idle_facts(),
                report: old,
                ..RecordingHostOps::default()
            },
            staged_local: staged,
            package_sha256: sha.clone(),
            uploaded_sha256: None,
            expected,
        };
        let mut journal = Journal::default();
        let err = apply_remote_package(dir.path(), &mut journal, &mut backend, &plan(&sha), true)
            .unwrap_err();
        assert!(err.contains("does not match staged remote-v*"));
        assert!(
            backend
                .ops
                .helper_actions
                .iter()
                .any(|action| action == "rollback"),
            "old still-running stack must roll back, not mark Applied"
        );
        assert!(!journal.already_applied("op-host-1"));
    }
}
