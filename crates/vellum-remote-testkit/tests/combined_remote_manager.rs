//! M16 combined Remote Manager acceptance (L1-L7).
//!
//! This suite is provider-billed and ignored by default. It drives the same
//! typed agent RPC sequence as Desktop's `RemoteHostManager`, then talks to the
//! resulting managed Broker through an SSH tunnel. Every case uses a unique
//! state root/profile and restores it during cleanup.

#![cfg(feature = "live-smoke")]

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use std::io::Write as _;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use ulid::Ulid;
use vellum_remote_protocol::{SubscriptionMode, ThreadEvent, ThreadRuntimeStatus};
use vellum_remote_testkit::app_server_events;
use vellum_remote_testkit::live::{
    LiveBrokerClient, LiveSmokeConfig, LiveSmokeError, SshTunnelGuard,
};

struct CombinedFixture {
    config: LiveSmokeConfig,
    agent_bin: String,
    broker_bin: String,
    state_root: String,
    profile_id: String,
    broker_port: u16,
    local_port: u16,
    proxy_port: u16,
    operation: String,
    tunnel: Option<SshTunnelGuard>,
    cleaned: bool,
}

impl CombinedFixture {
    async fn provision() -> Result<Self, LiveSmokeError> {
        let config = LiveSmokeConfig::from_env()?;
        let suffix = Ulid::new().to_string().to_lowercase();
        let profile_id = format!("m16-{suffix}");
        let operation = format!("m16-{suffix}");
        let state_root = format!(
            "{}/manager/{suffix}",
            config
                .remote_state_root
                .to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
        );
        let broker_port = env_u16("VELLUM_LIVE_MANAGER_BROKER_PORT", 45210)?;
        let local_port = env_u16("VELLUM_LIVE_MANAGER_LOCAL_PORT", 45210)?;
        // Never collide with the production Remote Manager proxy at 15721.
        let proxy_port = env_u16("VELLUM_LIVE_MANAGER_PROXY_PORT", 45721)?;
        let agent_bin =
            std::env::var("VELLUM_LIVE_AGENT_BIN").unwrap_or_else(|_| "vellum-remote-agent".into());
        let broker_bin = required_env("VELLUM_LIVE_REMOTE_BROKER_BIN")?;
        let mut fixture = Self {
            config,
            agent_bin,
            broker_bin,
            state_root,
            profile_id,
            broker_port,
            local_port,
            proxy_port,
            operation,
            tunnel: None,
            cleaned: false,
        };
        fixture.provision_stack().await?;
        fixture.tunnel = Some(
            SshTunnelGuard::open(
                &fixture.config.ssh_host,
                fixture.local_port,
                fixture.broker_port,
            )
            .await?,
        );
        Ok(fixture)
    }

    async fn provision_stack(&self) -> Result<(), LiveSmokeError> {
        let image = required_env("VELLUM_LIVE_PROXY_IMAGE")?;
        let config_toml = read_required("VELLUM_LIVE_PROXY_CONFIG")?;
        let catalog_json = read_required("VELLUM_LIVE_CATALOG_JSON")?;
        let credentials: Value =
            serde_json::from_str(&read_required("VELLUM_LIVE_CREDENTIALS_FILE")?)?;
        self.rpc(json!({"method": "agent.version"})).await?;
        self.rpc(json!({
            "method": "proxy.install",
            "operationId": format!("{}-install", self.operation),
            "image": image,
            "imageDigest": std::env::var("VELLUM_LIVE_PROXY_IMAGE_DIGEST").ok()
        }))
        .await?;
        for credential in credentials.as_array().ok_or_else(|| {
            LiveSmokeError::Message("VELLUM_LIVE_CREDENTIALS_FILE must contain an array".into())
        })? {
            let credential_id = credential["credentialId"].as_str().ok_or_else(|| {
                LiveSmokeError::Message("live credential entry is missing credentialId".into())
            })?;
            if let Some(secret) = credential.get("secret").and_then(Value::as_str) {
                self.rpc(json!({
                    "method": "credential.put",
                    "operationId": format!("{}-credential-{credential_id}", self.operation),
                    "credentialId": credential_id,
                    "secret": secret
                }))
                .await?;
            } else {
                let source_dir = required_env("VELLUM_LIVE_REMOTE_CREDENTIAL_DIR")?;
                self.put_remote_credential(&source_dir, credential_id)
                    .await?;
            }
        }
        self.rpc(json!({
            "method": "proxy.configure",
            "operationId": format!("{}-configure", self.operation),
            "configToml": config_toml
        }))
        .await?;
        let proxy = self
            .rpc(json!({
                "method": "proxy.start",
                "operationId": format!("{}-start", self.operation),
                "hostPort": self.proxy_port,
                "image": Value::Null
            }))
            .await?;
        if !proxy
            .pointer("/detail/ready")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(LiveSmokeError::Message(format!("proxy not ready: {proxy}")));
        }
        self.rpc(json!({
            "method": "codex.createManagedProfile",
            "operationId": format!("{}-profile", self.operation),
            "profileId": self.profile_id,
            "authJson": Value::Null
        }))
        .await?;
        self.rpc(json!({"method": "codex.planInjection", "profileId": self.profile_id}))
            .await?;
        self.rpc(json!({
            "method": "codex.inject",
            "operationId": format!("{}-inject", self.operation),
            "profileId": self.profile_id,
            "catalogJson": catalog_json
        }))
        .await?;
        let runtime = self
            .rpc(json!({
                "method": "codex.startManaged",
                "operationId": format!("{}-runtime", self.operation),
                "profileId": self.profile_id,
                "brokerPort": self.broker_port
            }))
            .await?;
        if !runtime
            .get("ready")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(LiveSmokeError::Message(format!(
                "managed runtime not ready: {runtime}"
            )));
        }
        Ok(())
    }

    async fn rpc(&self, request: Value) -> Result<Value, LiveSmokeError> {
        let command = format!(
            "VELLUM_CODEX_BIN={} VELLUM_REMOTE_BROKER_BIN={} {} --state-root {} rpc",
            shell_quote(&self.config.codex_native.to_string_lossy()),
            shell_quote(&self.broker_bin),
            shell_quote(&self.agent_bin),
            shell_quote(&self.state_root)
        );
        let mut child = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-T", &self.config.ssh_host, &command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| LiveSmokeError::Ssh(error.to_string()))?;
        child
            .stdin
            .take()
            .ok_or_else(|| LiveSmokeError::Ssh("agent stdin unavailable".into()))?
            .write_all(request.to_string().as_bytes())
            .await?;
        let output = tokio::time::timeout(Duration::from_secs(180), child.wait_with_output())
            .await
            .map_err(|_| LiveSmokeError::Timeout("remote-agent RPC".into()))??;
        let envelope: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            LiveSmokeError::Ssh(format!(
                "remote-agent exited {}; invalid stdout envelope ({error}); stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        })?;
        if envelope.get("type").and_then(Value::as_str) == Some("error") {
            return Err(LiveSmokeError::Protocol(envelope.to_string()));
        }
        if !output.status.success() {
            return Err(LiveSmokeError::Ssh(format!(
                "remote-agent exited {}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        envelope
            .get("result")
            .cloned()
            .ok_or_else(|| LiveSmokeError::Protocol(format!("missing result: {envelope}")))
    }

    /// Copy an existing credential into the isolated fixture without ever
    /// returning its plaintext to the Windows test runner. The remote Python
    /// process JSON-escapes the file directly into the agent RPC stdin; the
    /// agent response contains only the credential id and secret hash.
    async fn put_remote_credential(
        &self,
        source_dir: &str,
        credential_id: &str,
    ) -> Result<(), LiveSmokeError> {
        if credential_id.is_empty()
            || !credential_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err(LiveSmokeError::Isolation(format!(
                "invalid remote credential id: {credential_id}"
            )));
        }
        let source = format!("{}/{credential_id}", source_dir.trim_end_matches('/'));
        let operation_id = format!("{}-credential-{credential_id}", self.operation);
        let python = r#"import json,sys; print(json.dumps({'method':'credential.put','operationId':sys.argv[2],'credentialId':sys.argv[3],'secret':open(sys.argv[1],encoding='utf-8').read()}))"#;
        let agent_command = format!(
            "VELLUM_CODEX_BIN={} VELLUM_REMOTE_BROKER_BIN={} {} --state-root {} rpc",
            shell_quote(&self.config.codex_native.to_string_lossy()),
            shell_quote(&self.broker_bin),
            shell_quote(&self.agent_bin),
            shell_quote(&self.state_root)
        );
        let command = format!(
            "python3 -c {} {} {} {} | {agent_command}",
            shell_quote(python),
            shell_quote(&source),
            shell_quote(&operation_id),
            shell_quote(credential_id),
        );
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-T", &self.config.ssh_host, &command])
                .output(),
        )
        .await
        .map_err(|_| LiveSmokeError::Timeout("remote credential copy".into()))??;
        let envelope: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            LiveSmokeError::Ssh(format!(
                "remote credential RPC exited {}; invalid response ({error}); stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        })?;
        if !output.status.success() || envelope.get("type").and_then(Value::as_str) == Some("error")
        {
            return Err(LiveSmokeError::Protocol(format!(
                "remote credential RPC failed: {envelope}"
            )));
        }
        Ok(())
    }

    async fn client(&self, device: &str) -> Result<LiveBrokerClient, LiveSmokeError> {
        LiveBrokerClient::connect(&format!("ws://127.0.0.1:{}/ws", self.local_port), device).await
    }

    async fn read_remote_file(&self, path: &str) -> Result<String, LiveSmokeError> {
        let output = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-T",
                &self.config.ssh_host,
                &format!("cat -- {}", shell_quote(path)),
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Err(LiveSmokeError::Ssh(format!(
                "read remote file failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| LiveSmokeError::Protocol(format!("remote file is not UTF-8: {error}")))
    }

    /// Issue an HTTP request to the loopback Vellum proxy on the remote host
    /// (Gate 1 loopback lane). Fails closed when the proxy is unreachable.
    async fn proxy_http(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, LiveSmokeError> {
        let status = self.rpc(json!({"method": "proxy.status"})).await?;
        let port = status
            .pointer("/hostPort")
            .and_then(Value::as_u64)
            .ok_or_else(|| LiveSmokeError::Protocol("proxy hostPort missing".into()))?;
        let mut command =
            format!("curl -fsS --max-time 60 -X {method} 'http://127.0.0.1:{port}{path}'");
        if let Some(body) = body {
            command.push_str(&format!(
                " -H 'Content-Type: application/json' -d {}",
                shell_quote(body)
            ));
        }
        let output = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-T", &self.config.ssh_host, &command])
            .output()
            .await?;
        if !output.status.success() {
            return Err(LiveSmokeError::Ssh(format!(
                "proxy HTTP {method} {path} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| LiveSmokeError::Protocol(format!("proxy body is not UTF-8: {error}")))
    }

    async fn start_thread(
        &self,
        client: &LiveBrokerClient,
        model: &str,
    ) -> Result<String, LiveSmokeError> {
        client.hello(None).await?;
        let started = LiveBrokerClient::require_completed(
            client
                .thread_start(&self.state_root, Some(model.to_string()))
                .await?,
        )
        .await?;
        let thread = started.result["threadId"]
            .as_str()
            .ok_or_else(|| LiveSmokeError::Protocol("thread.start missing threadId".into()))?
            .to_string();
        client
            .subscribe_thread(&thread, 0, SubscriptionMode::Writer)
            .await?;
        LiveBrokerClient::require_completed(client.writer_acquire(&thread).await?).await?;
        Ok(thread)
    }

    async fn turn(
        &self,
        client: &LiveBrokerClient,
        thread: &str,
        prompt: &str,
    ) -> Result<u64, LiveSmokeError> {
        let after_seq = client.last_ack_seq().await;
        // Provider-billed turns can outlive the broker's 30-second writer
        // lease. Desktop renews before issuing a command; the live client must
        // exercise the same contract instead of relying on the lease obtained
        // during thread creation.
        LiveBrokerClient::require_completed(client.writer_acquire(thread).await?).await?;
        LiveBrokerClient::require_completed(client.turn_start(thread, prompt).await?).await?;
        let (status, seq) = client
            .wait_for_terminal_after(thread, after_seq, Duration::from_secs(300))
            .await?;
        if status != ThreadRuntimeStatus::Completed {
            return Err(LiveSmokeError::Protocol(format!(
                "turn ended as {status:?}"
            )));
        }
        Ok(seq)
    }

    /// Success-path teardown. Returns the per-step outcome so the test can
    /// report a teardown failure. `cleaned` is only set when every step
    /// succeeded; when any step failed it stays false so `Drop` retries the
    /// incomplete restore synchronously instead of leaving the host injected.
    async fn cleanup(mut self) -> Result<(), LiveSmokeError> {
        self.tunnel.take();
        let steps = self.restore_steps().await;
        if steps.all_ok() {
            self.cleaned = true;
            Ok(())
        } else {
            Err(LiveSmokeError::Message(format!(
                "teardown incomplete: {} failed; Drop will retry synchronously",
                steps.failed().join(", ")
            )))
        }
    }

    /// Stop the managed runtime, restore the Codex profile and stop the proxy.
    /// Each step is reported individually instead of being swallowed, so the
    /// caller can distinguish a fully restored host from a partial one.
    async fn restore_steps(&self) -> RestoreSteps {
        RestoreSteps {
            stop_managed: self
                .rpc(json!({
                    "method": "codex.stopManaged",
                    "operationId": format!("{}-cleanup-runtime", self.operation),
                    "profileId": self.profile_id
                }))
                .await
                .is_ok(),
            restore: self
                .rpc(json!({
                    "method": "codex.restore",
                    "operationId": format!("{}-cleanup-restore", self.operation),
                    "profileId": self.profile_id
                }))
                .await
                .is_ok(),
            proxy_stop: self
                .rpc(json!({
                    "method": "proxy.stop",
                    "operationId": format!("{}-cleanup-proxy", self.operation)
                }))
                .await
                .is_ok(),
        }
    }

    /// Synchronous best-effort restore used from `Drop`, so a panic, timeout
    /// or early `?` in a live test still returns the injected host to its
    /// original state instead of leaving a managed profile behind.
    fn drop_restore_best_effort(&self) {
        let command = format!(
            "VELLUM_CODEX_BIN={} VELLUM_REMOTE_BROKER_BIN={} {} --state-root {} rpc",
            shell_quote(&self.config.codex_native.to_string_lossy()),
            shell_quote(&self.broker_bin),
            shell_quote(&self.agent_bin),
            shell_quote(&self.state_root)
        );
        for request in [
            json!({
                "method": "codex.stopManaged",
                "operationId": format!("{}-drop-runtime", self.operation),
                "profileId": self.profile_id
            }),
            json!({
                "method": "codex.restore",
                "operationId": format!("{}-drop-restore", self.operation),
                "profileId": self.profile_id
            }),
            json!({
                "method": "proxy.stop",
                "operationId": format!("{}-drop-proxy", self.operation)
            }),
        ] {
            let mut child = match std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-T", &self.config.ssh_host, &command])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(_) => continue,
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(request.to_string().as_bytes());
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

/// Per-step outcome of the success-path restore. `Drop` retries any step that
/// did not complete, so a transient SSH failure cannot leave the injected
/// host behind unnoticed.
#[derive(Debug, Clone, Copy, Default)]
struct RestoreSteps {
    stop_managed: bool,
    restore: bool,
    proxy_stop: bool,
}

impl RestoreSteps {
    fn all_ok(self) -> bool {
        self.stop_managed && self.restore && self.proxy_stop
    }

    fn failed(self) -> Vec<&'static str> {
        let mut failed = Vec::new();
        if !self.stop_managed {
            failed.push("codex.stopManaged");
        }
        if !self.restore {
            failed.push("codex.restore");
        }
        if !self.proxy_stop {
            failed.push("proxy.stop");
        }
        failed
    }
}

impl Drop for CombinedFixture {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.drop_restore_best_effort();
    }
}

#[tokio::test]
#[ignore]
async fn l1_remote_proxy_basic_response() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l1").await.unwrap();
    let model = required_env("VELLUM_LIVE_OPENAI_COMPAT_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let seq = fixture
        .turn(&client, &thread, "Reply exactly VELLUM_M16_OK")
        .await
        .unwrap();
    assert_agent_message(&client, &thread, seq, None, "VELLUM_M16_OK")
        .await
        .unwrap();
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l2_remote_proxy_reasoning() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l2").await.unwrap();
    let model = required_env("VELLUM_LIVE_GROK_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let seq = fixture
        .turn(&client, &thread, "Reason briefly, then answer: 17 + 25")
        .await
        .unwrap();
    assert_agent_message(&client, &thread, seq, None, "42")
        .await
        .unwrap();
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l3_remote_proxy_tool_roundtrip() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l3").await.unwrap();
    let model = required_env("VELLUM_LIVE_OPENAI_COMPAT_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let turn1_seq = fixture
        .turn(
            &client,
            &thread,
            "Use shell_command to run printf VELLUM_TOOL_OK, then report it.",
        )
        .await
        .unwrap();
    // Structural evidence of real tool execution, not a prompt echo: an
    // `item/started` commandExecution for the typed tool plus the
    // `item/completed` commandExecution carrying the marker in its
    // aggregatedOutput, both inside the first turn's seq window.
    let call_event = wait_for_app_server_event(
        &client,
        &thread,
        turn1_seq,
        Duration::from_secs(10),
        |event| {
            app_server_events::is_command_started(event)
                && app_server_events::command(event)
                    .is_some_and(|command| command.contains("VELLUM_TOOL_OK"))
        },
    )
    .await
    .expect("L3 must emit a commandExecution item for the typed tool");
    let tool_event = wait_for_app_server_event(
        &client,
        &thread,
        turn1_seq,
        Duration::from_secs(10),
        |event| app_server_events::is_completed_command_output(event, "VELLUM_TOOL_OK"),
    )
    .await
    .expect("L3 must actually execute the command and produce the marker");
    assert!(tool_event.seq > call_event.seq);
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l4_remote_proxy_three_turn_continuation() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l4").await.unwrap();
    let model = required_env("VELLUM_LIVE_OPENAI_COMPAT_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    fixture
        .turn(&client, &thread, "Remember the number 731.")
        .await
        .unwrap();
    let second_seq = fixture
        .turn(&client, &thread, "Add 9 to the remembered number.")
        .await
        .unwrap();
    let final_seq = fixture
        .turn(&client, &thread, "State the resulting number only.")
        .await
        .unwrap();
    assert_agent_message(&client, &thread, final_seq, Some(second_seq), "740")
        .await
        .unwrap();
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l5_remote_proxy_restart_between_turns() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l5").await.unwrap();
    let model = required_env("VELLUM_LIVE_OPENAI_COMPAT_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let first_seq = fixture
        .turn(&client, &thread, "Remember restart marker 8841.")
        .await
        .unwrap();
    let before = fixture
        .rpc(json!({"method": "proxy.status"}))
        .await
        .unwrap();
    fixture.rpc(json!({"method": "proxy.restart", "operationId": format!("{}-l5-restart", fixture.operation)})).await.unwrap();
    let after = fixture
        .rpc(json!({"method": "proxy.status"}))
        .await
        .unwrap();
    assert_eq!(before["installId"], after["installId"]);
    let final_seq = fixture
        .turn(&client, &thread, "Return the restart marker.")
        .await
        .unwrap();
    assert_agent_message(&client, &thread, final_seq, Some(first_seq), "8841")
        .await
        .unwrap();
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l6_remote_proxy_compact_then_continue() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let client = fixture.client("m16-l6").await.unwrap();
    let model = required_env("VELLUM_LIVE_COMPACTION_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let prompt =
        std::fs::read_to_string(required_env("VELLUM_LIVE_COMPACTION_PROMPT").unwrap()).unwrap();
    let compact_seq = fixture.turn(&client, &thread, &prompt).await.unwrap();
    // The large first user item is pending input, not active history. The
    // second turn hydrates it into active history and is therefore the point
    // where automatic canonical compaction must run.
    let continuation_seq = fixture
        .turn(
            &client,
            &thread,
            "Continue using the compacted context and state its checkpoint marker.",
        )
        .await
        .unwrap();
    assert_agent_message(
        &client,
        &thread,
        continuation_seq,
        Some(compact_seq),
        "VELLUM_COMPACT_CHECKPOINT",
    )
    .await
    .unwrap();
    let usage = fixture
        .read_remote_file(&format!("{}/proxy/data/usage.jsonl", fixture.state_root))
        .await
        .unwrap();
    assert!(
        usage
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|record| {
                record
                    .get("compactionTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
            }),
        "L6 must record non-zero compaction usage on the continuation turn"
    );
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

#[tokio::test]
#[ignore]
async fn l7_remote_proxy_desktop_disconnect_reconnect() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let device = "m16-l7";
    let client = fixture.client(device).await.unwrap();
    let model = required_env("VELLUM_LIVE_OPENAI_COMPAT_MODEL").unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let before = fixture.rpc(json!({"method": "host.status"})).await.unwrap();
    let _ = client
        .turn_start(&thread, "Run sleep 8, then reply VELLUM_DETACH_OK.")
        .await
        .unwrap();
    let _ = client
        .wait_for_event(Duration::from_secs(60), |event| {
            event.method.contains("turn/started")
                || event.method.contains("item/")
                || event.method.contains("command")
        })
        .await;
    let cursor = client.last_ack_seq().await;
    assert!(cursor > 0, "expected remote progress before disconnect");
    client.disconnect_abrupt().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let reconnected = fixture.client(device).await.unwrap();
    reconnected.hello(None).await.unwrap();
    let recovered = reconnected
        .recover_thread(
            &thread,
            cursor,
            SubscriptionMode::Observer,
            Duration::from_secs(300),
        )
        .await
        .unwrap();
    assert_eq!(recovered.terminal, Some(ThreadRuntimeStatus::Completed));
    assert_agent_message(
        &reconnected,
        &thread,
        recovered.final_seq,
        Some(cursor),
        "VELLUM_DETACH_OK",
    )
    .await
    .unwrap();
    let after = fixture.rpc(json!({"method": "host.status"})).await.unwrap();
    assert_eq!(
        before.pointer("/proxy/installId"),
        after.pointer("/proxy/installId")
    );
    assert_eq!(
        before.pointer("/proxy/containerId"),
        after.pointer("/proxy/containerId")
    );
    assert_eq!(
        before.pointer("/proxy/configHash"),
        after.pointer("/proxy/configHash")
    );
    reconnected.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

/// Gate 1: e806 transport qualification.
///
/// e806 is a remote OpenAI-compatible API (`openAiCompatible + bearer +
/// chat`, `server_side_resume=false`). It only runs wiring and protocol
/// tests, never capability benchmarks: the promotion label for this lane is
/// `API transport qualified` / `Codex protocol qualified` and must not claim
/// HumanEval/SWE results. Requires the same live env as L1-L7 plus
/// `VELLUM_LIVE_E806_MODEL` (the selected e806 catalog id).
#[tokio::test]
#[ignore]
async fn l8_e806_transport_qualification() {
    let fixture = CombinedFixture::provision().await.unwrap();
    let model = required_env("VELLUM_LIVE_E806_MODEL").unwrap();

    // 1. `/v1/models` exposes only the selected catalog model, never weikuwu.
    let models = fixture.proxy_http("GET", "/v1/models", None).await.unwrap();
    assert!(
        models.contains(&model),
        "e806 catalog model missing from /v1/models: {models}"
    );
    assert!(
        !models.to_ascii_lowercase().contains("weikuwu"),
        "retired route leaked into /v1/models: {models}"
    );

    // 2. Non-streaming marker request through the loopback proxy.
    let marker_body = serde_json::json!({
        "model": model,
        "input": "Reply with exactly VELLUM_E806_OK",
        "stream": false
    })
    .to_string();
    let marker = fixture
        .proxy_http("POST", "/v1/responses", Some(&marker_body))
        .await
        .unwrap();
    assert!(
        marker.contains("VELLUM_E806_OK"),
        "non-streaming marker mismatch: {marker}"
    );
    assert!(
        marker.contains("\"usage\"") && marker.contains("input_tokens"),
        "normalized usage missing from marker response: {marker}"
    );

    // 3. SSE marker request must terminate with a terminal event, never a
    // failed frame or a silent hang.
    let stream_body = serde_json::json!({
        "model": model,
        "input": "Reply with exactly VELLUM_E806_SSE_OK",
        "stream": true
    })
    .to_string();
    let stream = fixture
        .proxy_http("POST", "/v1/responses", Some(&stream_body))
        .await
        .unwrap();
    assert!(
        stream.contains("response.completed") || stream.contains("[DONE]"),
        "SSE stream never reached a terminal event: {stream}"
    );
    assert!(
        !stream.contains("response.failed"),
        "SSE stream failed closed: {stream}"
    );

    // 4-5. Typed tool request + tool-result continuation through the Broker
    // lane (Codex app-server notifications), same thread, no provider
    // response-id handoff. The evidence is structural: an `item/started`
    // commandExecution item proves the model asked for the typed tool, and
    // the `item/completed` commandExecution with the marker inside its
    // aggregatedOutput (status completed, exit code 0) proves the host
    // actually ran it. A `userMessage` prompt echo can never forge either
    // shape, and the append-only per-thread seq window plus the payload
    // `turnId` pin both events to the first turn.
    let client = fixture.client("m16-l8").await.unwrap();
    let thread = fixture.start_thread(&client, &model).await.unwrap();
    let turn1_seq = fixture
        .turn(
            &client,
            &thread,
            "Use shell_command to run printf VELLUM_E806_TOOL_OK, then report it.",
        )
        .await
        .unwrap();
    let turn1_id = first_turn_id(&client, &thread, turn1_seq).await.unwrap();
    let call_event = wait_for_app_server_event(
        &client,
        &thread,
        turn1_seq,
        Duration::from_secs(10),
        |event| {
            app_server_events::is_command_started(event)
                && app_server_events::command(event)
                    .is_some_and(|command| command.contains("VELLUM_E806_TOOL_OK"))
        },
    )
    .await
    .expect("first turn must emit a commandExecution item for the typed tool");
    assert_eq!(
        call_event.turn_id.as_deref(),
        Some(turn1_id.as_str()),
        "commandExecution must belong to the first turn"
    );
    let tool_event = wait_for_app_server_event(
        &client,
        &thread,
        turn1_seq,
        Duration::from_secs(10),
        |event| app_server_events::is_completed_command_output(event, "VELLUM_E806_TOOL_OK"),
    )
    .await
    .expect("first turn must complete the command with the marker in its output");
    assert!(
        tool_event.seq > call_event.seq,
        "tool result must follow the typed tool call (call seq {} >= result seq {})",
        call_event.seq,
        tool_event.seq
    );
    assert_eq!(
        tool_event.thread_id, thread,
        "tool event must belong to the started thread: {:?}",
        tool_event.thread_id
    );
    assert_eq!(
        tool_event.turn_id.as_deref(),
        Some(turn1_id.as_str()),
        "tool result must belong to the first turn"
    );

    let turn2_seq = fixture
        .turn(
            &client,
            &thread,
            "Continue the same task on this thread: state the marker you produced and add 1.",
        )
        .await
        .unwrap();
    let turn2_id = first_turn_id(&client, &thread, turn2_seq).await.unwrap();
    assert_ne!(turn2_id, turn1_id, "continuation must start a new turn");
    // The continuation turn must restate the turn-1 marker in an `agentMessage`
    // item on the same thread (same workspace, canonical history), proving
    // same-thread resume rather than a fresh stateless answer. The turn-2
    // window (`turn1_seq < seq <= turn2_seq`) excludes the turn-1 events, and
    // only the model's own `agentMessage` item can match.
    let continued_event = wait_for_app_server_event(
        &client,
        &thread,
        turn2_seq,
        Duration::from_secs(10),
        |event| {
            event.seq > turn1_seq
                && app_server_events::is_agent_message_containing(event, "VELLUM_E806_TOOL_OK")
        },
    )
    .await
    .expect("continuation turn must restate the turn-1 marker in an agentMessage item");
    assert_eq!(continued_event.thread_id, thread);
    assert_eq!(
        continued_event.turn_id.as_deref(),
        Some(turn2_id.as_str()),
        "continuation message must belong to the second turn"
    );
    client.close().await;
    fixture.cleanup().await.expect("live teardown failed");
}

/// M34: manual detach/resume acceptance observer.
///
/// Observes the production native daemon without provisioning a managed
/// profile or Broker. At the printed steps the operator closes and reopens
/// the Windows Codex App; the observer verifies that the same native daemon,
/// running turn and thread survive. The harness never closes the App itself.
#[tokio::test]
#[ignore]
async fn l9_manual_detach_resume_observer() {
    // M34 P0 fix: a read-only observer of the *production native app-server
    // daemon*. It provisions no managed profile and no Broker; the session
    // under test is created by Codex App itself, and every observation comes
    // from the agent's `codex.sessionStatus` control-API query. Closing the
    // Codex App therefore has a direct causal relationship with the session
    // being observed.
    let alias = required_env("VELLUM_LIVE_HOST_ALIAS").unwrap();
    let agent_bin = std::env::var("VELLUM_LIVE_AGENT_BIN")
        .unwrap_or_else(|_| "vellum-remote-agent".to_string());

    // Baseline: wait for a *running* native turn the operator starts in Codex
    // App (thread status active + active turn id present). Never accept a
    // completed turn as baseline: that would only prove persisted history.
    println!(
        "\nMANUAL STEP 1 (l9): in the Codex App, start a turn on the remote host that\n\
         will keep working for at least 60s (e.g. \"run sleep 60 then reply DETACH_OK\").\n\
         Press Enter once the turn is visibly running (not completed)."
    );
    wait_for_manual_enter();
    let mut baseline = None;
    let baseline_deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < baseline_deadline {
        let status = native_session_status(&alias, &agent_bin, None).await;
        let active = status
            .pointer("/threads")
            .and_then(Value::as_array)
            .and_then(|threads| {
                threads
                    .iter()
                    .find(|thread| thread.get("active").and_then(Value::as_bool) == Some(true))
            });
        if let Some(active) = active {
            baseline = Some((
                active
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                active
                    .get("activeTurnId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                active.get("turnCount").and_then(Value::as_u64).unwrap_or(0),
                status.pointer("/daemonPid").and_then(Value::as_u64),
            ));
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let (thread_id, active_turn_id, baseline_turns, daemon_pid) =
        baseline.expect("no running native turn observed in 120s; start one in Codex App");
    assert!(
        active_turn_id.is_some(),
        "baseline must have an active turn id, not just completed history"
    );
    println!(
        "baseline: thread {thread_id} turn {:?} turns {baseline_turns} daemon pid {daemon_pid:?}",
        active_turn_id
    );

    println!(
        "\nMANUAL STEP 2 (l9): close the Windows Codex App now.\n\
         The remote daemon must keep running and the turn must keep making progress.\n\
         Press Enter once the App is fully closed."
    );
    wait_for_manual_enter();

    // While the App is closed, observe ONLY the native daemon. The turn must
    // either advance (turn count grows) or complete; cancelled/interrupted is
    // a hard failure, and the daemon identity must not change.
    let progress_deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut progressed = false;
    // A completion observed on the very first post-close poll is inconclusive:
    // the turn may have finished before the App actually disconnected. Only a
    // turn that is still `inProgress` *after* the operator confirmed closure,
    // and later advances or completes, proves detach continuation.
    let mut observed_in_progress_after_close = false;
    while std::time::Instant::now() < progress_deadline {
        let status = native_session_status(&alias, &agent_bin, Some(&thread_id)).await;
        let threads = status
            .pointer("/threads")
            .and_then(Value::as_array)
            .cloned();
        let thread = threads
            .as_ref()
            .and_then(|threads| {
                threads.iter().find(|entry| {
                    entry.get("threadId").and_then(Value::as_str) == Some(thread_id.as_str())
                })
            })
            .cloned();
        let Some(thread) = thread else {
            panic!("native thread {thread_id} disappeared while the App was closed: {status}");
        };
        assert_eq!(
            status.pointer("/daemonPid").and_then(Value::as_u64),
            daemon_pid,
            "daemon must not be restarted by closing the App"
        );
        let active_turn_now = thread
            .get("activeTurnId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (Some(expected_turn), Some(active_turn_now)) =
            (active_turn_id.as_deref(), active_turn_now.as_deref())
        {
            assert_eq!(
                expected_turn, active_turn_now,
                "the observed turn must stay the same while the App is closed"
            );
        }
        let turn_count = thread.get("turnCount").and_then(Value::as_u64).unwrap_or(0);
        let last_turn_status = thread
            .get("lastTurnStatus")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            last_turn_status != "interrupted" && last_turn_status != "failed",
            "turn was interrupted/failed while the App was closed: {thread}"
        );
        if last_turn_status == "inProgress" {
            observed_in_progress_after_close = true;
        }
        if turn_count > baseline_turns || last_turn_status == "completed" {
            assert!(
                observed_in_progress_after_close,
                "inconclusive: the turn was already completed/finished before the \
                 App close was observed; rerun with a longer-running turn"
            );
            progressed = true;
            println!(
                "remote progress while App closed: turns {baseline_turns} -> {turn_count}, \
                 last turn status {last_turn_status}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    assert!(
        progressed,
        "no native progress observed while the App was closed (300s)"
    );

    println!(
        "\nMANUAL STEP 3 (l9): reopen the Codex App and press Enter once the thread is visible."
    );
    wait_for_manual_enter();
    let final_status = native_session_status(&alias, &agent_bin, Some(&thread_id)).await;
    let final_thread = final_status
        .pointer("/threads")
        .and_then(Value::as_array)
        .and_then(|threads| {
            threads.iter().find(|entry| {
                entry.get("threadId").and_then(Value::as_str) == Some(thread_id.as_str())
            })
        })
        .cloned();
    assert!(
        final_thread.is_some(),
        "same native thread must be visible after reopen: {final_status}"
    );
    println!("l9 PASS: native thread {thread_id} survived App close and resumed.");
}

async fn native_session_status(alias: &str, agent_bin: &str, thread_id: Option<&str>) -> Value {
    use tokio::io::AsyncWriteExt as _;
    let mut request = json!({"method": "codex.sessionStatus"});
    if let Some(thread_id) = thread_id {
        request["threadId"] = json!(thread_id);
    }
    let mut child = tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-T", alias, agent_bin, "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ssh agent rpc");
    child
        .stdin
        .take()
        .expect("agent stdin")
        .write_all(request.to_string().as_bytes())
        .await
        .expect("write agent request");
    let output = child.wait_with_output().await.expect("wait agent rpc");
    assert!(
        output.status.success(),
        "agent rpc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("agent envelope");
    assert_eq!(
        envelope.get("type").and_then(Value::as_str),
        Some("ok"),
        "codex.sessionStatus must answer, not fall back: {envelope}"
    );
    envelope.get("result").cloned().unwrap_or(envelope)
}

fn wait_for_manual_enter() {
    use std::io::BufRead as _;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("read the manual confirmation line");
}

/// Scan the append-only per-thread event log for the last event matching
/// `predicate` within `max_seq` (inclusive). The seq bound partitions turns:
/// the broker seq is a per-thread append-only cursor, so `seq <= turn1_seq`
/// selects exactly the first turn's events and `turn1_seq < seq <= turn2_seq`
/// selects the second turn's. `Timeout` ends the scan, so a missing event is
/// a hard protocol failure, not a race.
///
/// Predicates live in `vellum_remote_testkit::app_server_events` and judge
/// app-server notification shapes (`item/started`, `item/completed`, typed
/// `ThreadItem` payloads), never serialized-JSON substrings.
async fn wait_for_app_server_event(
    client: &LiveBrokerClient,
    thread: &str,
    max_seq: u64,
    idle: Duration,
    predicate: impl Fn(&ThreadEvent) -> bool,
) -> Result<ThreadEvent, LiveSmokeError> {
    let mut found: Option<ThreadEvent> = None;
    loop {
        let after = found.as_ref().map(|event| event.seq);
        match client
            .wait_for_event(idle, |event| {
                vellum_remote_testkit::app_server_events::in_turn_window(
                    event, thread, after, max_seq,
                ) && predicate(event)
            })
            .await
        {
            Ok(event) => found = Some(event),
            Err(LiveSmokeError::Timeout(_)) => break,
            Err(error) => return Err(error),
        }
    }
    found.ok_or_else(|| LiveSmokeError::Protocol("no matching app-server event".into()))
}

/// Require model-authored text in a bounded turn window. This accepts only
/// typed app-server `agentMessage` items, so user prompts, reasoning records,
/// raw SSE frames and serialized JSON substrings cannot satisfy the check.
async fn assert_agent_message(
    client: &LiveBrokerClient,
    thread: &str,
    max_seq: u64,
    after_seq: Option<u64>,
    needle: &str,
) -> Result<ThreadEvent, LiveSmokeError> {
    wait_for_app_server_event(client, thread, max_seq, Duration::from_secs(10), |event| {
        after_seq.is_none_or(|after| event.seq > after)
            && app_server_events::is_agent_message_containing(event, needle)
    })
    .await
    .map_err(|error| {
        LiveSmokeError::Protocol(format!(
            "missing agentMessage containing {needle:?} in seq window {:?}..={max_seq}: {error}",
            after_seq
        ))
    })
}

/// The `turnId` of the first `turn/started` notification within `max_seq`.
/// App-server item events carry `turnId` in their payload (broker extracts it
/// into `ThreadEvent.turn_id`), so this gives the authoritative turn
/// attribution for the first turn.
async fn first_turn_id(
    client: &LiveBrokerClient,
    thread: &str,
    max_seq: u64,
) -> Result<String, LiveSmokeError> {
    let event =
        wait_for_app_server_event(client, thread, max_seq, Duration::from_secs(10), |event| {
            event.method == "turn/started"
        })
        .await?;
    event
        .turn_id
        .clone()
        .ok_or_else(|| LiveSmokeError::Protocol("turn/started without turnId".into()))
}

fn required_env(name: &str) -> Result<String, LiveSmokeError> {
    std::env::var(name).map_err(|_| LiveSmokeError::Message(format!("{name} is required")))
}

fn read_required(name: &str) -> Result<String, LiveSmokeError> {
    let path = required_env(name)?;
    std::fs::read_to_string(&path)
        .map_err(|error| LiveSmokeError::Message(format!("read {name}={path}: {error}")))
}

fn env_u16(name: &str, default: u16) -> Result<u16, LiveSmokeError> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .map_err(|_| LiveSmokeError::Message(format!("invalid {name}")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
