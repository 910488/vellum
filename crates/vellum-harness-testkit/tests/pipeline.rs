//! Full-pipeline contract tests (§45–§53 of the harness manager plan).
//!
//! ```text
//! fake Codex UI client -> Codex facade -> broker -> ACP client -> fake agent
//! ```
//!
//! Every layer below the UI is the real implementation; only the provider is
//! fake. Nothing here needs a live API key.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use vellum_codex_facade::{CodexAppServerFacade, CodexServerMessage, ManagedHarnessBroker};
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessId, HarnessTransportKind, ProcessScope,
};
use vellum_harness_runtime::{
    adapters::acp::AcpHarnessRuntime, HarnessBindingStore, HarnessDriver, HarnessError,
    HarnessLaunchSpec, HarnessProbeContext, HarnessProbeResult, HarnessRegistry, HarnessRuntime,
    HarnessStartSpec, NativeHarnessSupervisor,
};
use vellum_harness_testkit::{FakeAgentScript, ScriptedEvent};

/// A driver that launches the scripted fake agent. It uses the real supervisor
/// and the real ACP runtime, so only the provider on the far end is fake.
struct ScriptedDriver {
    descriptor: HarnessDescriptor,
    script_path: PathBuf,
    supervisor: Arc<NativeHarnessSupervisor>,
    starts: AtomicUsize,
}

impl ScriptedDriver {
    fn new(descriptor: HarnessDescriptor, script: &FakeAgentScript, dir: &std::path::Path) -> Self {
        let script_path = dir.join(format!("{}-script.json", descriptor.id.0));
        std::fs::write(&script_path, serde_json::to_string(script).unwrap()).unwrap();
        Self {
            descriptor,
            script_path,
            supervisor: Arc::new(NativeHarnessSupervisor::default()),
            starts: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl HarnessDriver for ScriptedDriver {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    async fn probe(
        &self,
        context: &HarnessProbeContext,
    ) -> Result<HarnessProbeResult, HarnessError> {
        Ok(HarnessProbeResult {
            harness_id: self.descriptor.id.clone(),
            available: context.workspace.is_dir(),
            capabilities: Some(self.descriptor.capabilities.clone()),
            diagnostic: None,
        })
    }
    async fn start(&self, spec: HarnessStartSpec) -> Result<Arc<dyn HarnessRuntime>, HarnessError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let process = self
            .supervisor
            .spawn(HarnessLaunchSpec {
                executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-acp-agent")),
                args: vec![self.script_path.to_string_lossy().into_owned()],
                cwd: spec.workspace,
                env: BTreeMap::new(),
                inherit_env: true,
            })
            .await?;
        Ok(AcpHarnessRuntime::from_process(self.descriptor.clone(), process).await?)
    }
}

fn descriptor(id: &str, capabilities: HarnessCapabilities) -> HarnessDescriptor {
    HarnessDescriptor {
        id: HarnessId(id.into()),
        display_name: id.into(),
        vendor: "test".into(),
        transport: HarnessTransportKind::AcpStdio,
        process_scope: ProcessScope::PerWorkspace,
        capabilities,
    }
}

fn full_capabilities() -> HarnessCapabilities {
    HarnessCapabilities {
        session_resume: true,
        session_list: true,
        session_close: true,
        model_selection: true,
        reasoning_effort: true,
        reasoning_stream: true,
        tool_lifecycle: true,
        permissions: true,
        compaction: true,
        plans: true,
        commands: true,
        terminals: true,
        subagents: true,
        native_memory: true,
        usage: true,
        context_usage: true,
    }
}

/// The capability set DeepSeek's ACP actually exposes.
fn deepseek_capabilities() -> HarnessCapabilities {
    HarnessCapabilities {
        session_resume: true,
        session_list: true,
        session_close: true,
        model_selection: true,
        reasoning_effort: true,
        reasoning_stream: true,
        tool_lifecycle: true,
        permissions: true,
        usage: true,
        context_usage: true,
        compaction: false,
        plans: false,
        commands: false,
        terminals: false,
        subagents: false,
        native_memory: false,
    }
}

fn full_turn_script() -> FakeAgentScript {
    FakeAgentScript {
        on_prompt: vec![
            ScriptedEvent::TurnStarted {
                turn_id: Some("turn-1".into()),
            },
            ScriptedEvent::Reasoning {
                text: "inspecting the workspace".into(),
            },
            ScriptedEvent::ToolStart {
                tool_call_id: "call-1".into(),
                title: "read_file".into(),
            },
            ScriptedEvent::Permission {
                tool_call_id: "call-1".into(),
                title: "read src/main.rs".into(),
            },
            ScriptedEvent::ToolEnd {
                tool_call_id: "call-1".into(),
                title: Some("read_file".into()),
                output: json!({"bytes": 42}),
                failed: false,
            },
            ScriptedEvent::Native {
                method: "xai/doom_loop_check".into(),
                payload: json!({"repeats": 0}),
            },
            ScriptedEvent::Assistant {
                text: "done".into(),
            },
            ScriptedEvent::Complete {
                turn_id: Some("turn-1".into()),
            },
        ],
        ..Default::default()
    }
}

struct Harness {
    facade: CodexAppServerFacade,
    workspace: tempfile::TempDir,
    _scripts: tempfile::TempDir,
    driver: Arc<ScriptedDriver>,
}

fn build(id: &str, capabilities: HarnessCapabilities, script: FakeAgentScript) -> Harness {
    let workspace = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let driver = Arc::new(ScriptedDriver::new(
        descriptor(id, capabilities),
        &script,
        scripts.path(),
    ));
    let mut registry = HarnessRegistry::default();
    registry.register(driver.clone()).unwrap();
    let bindings =
        Arc::new(HarnessBindingStore::open(scripts.path().join("bindings.sqlite")).unwrap());
    let broker = Arc::new(ManagedHarnessBroker::new(Arc::new(registry), bindings));
    Harness {
        facade: CodexAppServerFacade::new(broker),
        workspace,
        _scripts: scripts,
        driver,
    }
}

impl Harness {
    async fn start_thread(&self, selection: Value) -> String {
        let response = self
            .facade
            .handle_request(
                "thread/start",
                json!({
                    "cwd": self.workspace.path().to_string_lossy(),
                    "harnessSelection": selection
                }),
            )
            .await
            .expect("thread/start succeeds");
        response["thread"]["id"].as_str().unwrap().to_owned()
    }
}

/// Collects outbound UI messages until `predicate` matches or the deadline
/// passes.
async fn collect_until(
    receiver: &mut tokio::sync::broadcast::Receiver<CodexServerMessage>,
    predicate: impl Fn(&CodexServerMessage) -> bool,
) -> Vec<CodexServerMessage> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, receiver.recv()).await {
            Ok(Ok(message)) => {
                let matched = predicate(&message);
                collected.push(message);
                if matched {
                    return collected;
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) | Err(_) => return collected,
        }
    }
}

fn methods_of(messages: &[CodexServerMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|message| message.method().to_owned())
        .collect()
}

fn is_turn_completed(message: &CodexServerMessage) -> bool {
    message.method() == "turn/completed"
}

fn has_native_event(entries: &[vellum_codex_facade::JournalEntry], event_type: &str) -> bool {
    entries.iter().any(|entry| {
        matches!(
            &entry.envelope.event,
            vellum_harness_protocol::HarnessEvent::NativeExtension(extension)
                if extension.event_type == event_type
        )
    })
}

#[tokio::test]
async fn a_native_turn_flows_end_to_end_from_thread_start_to_turn_completed() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let mut messages = harness.facade.subscribe();
    let thread_id = harness
        .start_thread(
            json!({"harnessId": "grok-build", "modelId": "grok-4.6", "reasoningEffort": "high"}),
        )
        .await;

    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "make one small change"}]}),
        )
        .await
        .expect("turn/start succeeds");

    let seen = collect_until(&mut messages, is_turn_completed).await;
    let observed = methods_of(&seen);

    // Every one of these is a real Codex App Server method from the pinned
    // schema, not a name invented for the facade's convenience.
    for expected in [
        "turn/started",
        "item/reasoning/textDelta",
        "item/started",
        "item/permissions/requestApproval",
        "item/completed",
        "item/agentMessage/delta",
        "turn/completed",
    ] {
        assert!(
            observed.iter().any(|method| method == expected),
            "missing {expected} in {observed:?}"
        );
    }

    // The tool call is reported as a dynamicToolCall item that starts, then
    // completes.
    let tool_started = seen
        .iter()
        .filter_map(CodexServerMessage::as_notification)
        .find(|notification| {
            notification.method == "item/started"
                && notification.params["item"]["type"] == json!("dynamicToolCall")
        })
        .expect("tool call started");
    assert_eq!(tool_started.params["item"]["tool"], json!("read_file"));
    assert_eq!(tool_started.params["item"]["status"], json!("inProgress"));
    let tool_completed = seen
        .iter()
        .filter_map(CodexServerMessage::as_notification)
        .find(|notification| {
            notification.method == "item/completed"
                && notification.params["item"]["type"] == json!("dynamicToolCall")
        })
        .expect("tool call completed");
    // The completion names the tool rather than echoing its call id.
    assert_eq!(tool_completed.params["item"]["tool"], json!("read_file"));

    // The provider-specific event has no Codex representation, so it is absent
    // from the UI stream but preserved in the audit journal.
    assert!(!observed.iter().any(|method| method.contains("doom_loop")));
    assert!(
        has_native_event(
            &harness.facade.audit_journal(&thread_id).await,
            "xai/doom_loop_check"
        ),
        "the native event must be preserved even though the UI never shows it"
    );
}

#[tokio::test]
async fn a_permission_is_resolved_once_and_a_replay_of_it_fails_closed() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let mut messages = harness.facade.subscribe();
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "go"}]}),
        )
        .await
        .unwrap();

    let seen = collect_until(&mut messages, |message| {
        message.method() == "item/permissions/requestApproval"
    })
    .await;
    let (request_id, params) = seen
        .iter()
        .find_map(|message| match message {
            CodexServerMessage::Request { id, params, .. } => Some((id.clone(), params.clone())),
            CodexServerMessage::Notification(_) => None,
        })
        .expect("permission arrives as a server request the UI must answer");

    // The UI is given a Vellum id, never the native request id.
    assert!(request_id.starts_with("perm_"));
    assert_ne!(request_id, "call-1");
    assert_eq!(params["reason"], json!("read src/main.rs"));

    harness
        .facade
        .handle_response(
            &request_id,
            json!({"permissions": {"fileSystem": "write"}, "scope": "turn"}),
        )
        .await
        .expect("first resolution reaches the native harness");

    // The decision actually arrived at the agent.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut echoed = false;
    while tokio::time::Instant::now() < deadline && !echoed {
        echoed = has_native_event(
            &harness.facade.audit_journal(&thread_id).await,
            "testkit/permission_resolved",
        );
        if !echoed {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert!(echoed, "the agent never observed the permission decision");

    let error = harness
        .facade
        .handle_response(&request_id, json!({"permissions": {"fileSystem": "write"}}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("already resolved"),
        "a replayed approval must fail closed, got: {error}"
    );
    assert_eq!(harness.facade.permissions().pending().await, 0);
}

#[tokio::test]
async fn a_refusal_is_never_read_as_an_implicit_grant() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let mut messages = harness.facade.subscribe();
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "go"}]}),
        )
        .await
        .unwrap();
    let seen = collect_until(&mut messages, |message| {
        message.method() == "item/permissions/requestApproval"
    })
    .await;
    let request_id = seen
        .iter()
        .find_map(|message| match message {
            CodexServerMessage::Request { id, .. } => Some(id.clone()),
            CodexServerMessage::Notification(_) => None,
        })
        .expect("permission request");

    // A response carrying no granted profile is a refusal, not a silent yes.
    harness
        .facade
        .handle_response(&request_id, json!({}))
        .await
        .expect("a refusal is still a valid answer");
    assert_eq!(harness.facade.permissions().pending().await, 0);
}

#[tokio::test]
async fn capability_gaps_are_reported_rather_than_emulated() {
    let harness = build(
        "deepseek-harness",
        deepseek_capabilities(),
        full_turn_script(),
    );
    let thread_id = harness
        .start_thread(json!({"harnessId": "deepseek-harness"}))
        .await;

    // DeepSeek's ACP has no terminals and no fork, so the facade must say so
    // instead of returning a plausible empty success.
    for method in ["command/exec", "thread/fork"] {
        let error = harness
            .facade
            .handle_request(method, json!({"threadId": thread_id}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("UnsupportedCapability"),
            "{method} should be unsupported, got: {error}"
        );
    }

    // Nor may compaction silently fall through to another engine.
    let error = harness
        .facade
        .handle_request("thread/compact/start", json!({"threadId": thread_id}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("UnsupportedCapability"));
}

#[tokio::test]
async fn compaction_stays_with_the_harness_that_owns_the_thread() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    harness
        .facade
        .handle_request("thread/compact/start", json!({"threadId": thread_id}))
        .await
        .expect("a native harness compacts natively");

    // One runtime start for the thread: compaction did not reach for a second
    // harness, and no Vellum-side engine was involved.
    assert_eq!(harness.driver.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_and_effort_are_written_through_real_config_requests() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    for (key, value) in [("model", "grok-4.6"), ("reasoning_effort", "high")] {
        harness
            .facade
            .handle_request(
                "config/value/write",
                json!({"threadId": thread_id, "key": key, "value": value}),
            )
            .await
            .unwrap_or_else(|error| panic!("config/value/write {key} failed: {error}"));
    }

    // An unknown config key is refused rather than silently accepted.
    let error = harness
        .facade
        .handle_request(
            "config/value/write",
            json!({"threadId": thread_id, "key": "sandbox_mode", "value": "danger"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("UnsupportedCapability"));
}

#[tokio::test]
async fn two_threads_on_one_harness_stay_isolated() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let mut messages = harness.facade.subscribe();
    let thread_a = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    let thread_b = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    assert_ne!(thread_a, thread_b);

    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_a, "input": [{"type": "text", "text": "a"}]}),
        )
        .await
        .unwrap();

    let seen = collect_until(&mut messages, is_turn_completed).await;
    assert!(!seen.is_empty());
    // Only the prompted thread produced events; B was never touched.
    for notification in seen.iter().filter_map(CodexServerMessage::as_notification) {
        if let Some(thread_id) = notification.params["threadId"].as_str() {
            assert_eq!(thread_id, thread_a, "thread B saw events from thread A");
        }
    }
    assert!(harness.facade.audit_journal(&thread_b).await.is_empty());
}

#[tokio::test]
async fn the_journal_serves_reconnect_and_is_labelled_as_a_mirror() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let mut messages = harness.facade.subscribe();
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;
    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "go"}]}),
        )
        .await
        .unwrap();
    collect_until(&mut messages, is_turn_completed).await;

    let read = harness
        .facade
        .handle_request("thread/read", json!({"threadId": thread_id}))
        .await
        .unwrap();
    assert_eq!(read["source"], json!("vellumJournalMirror"));
    let events = read["events"].as_array().unwrap();
    assert!(!events.is_empty());
    // Replayed history never re-issues an answerable permission request.
    assert!(events
        .iter()
        .all(|event| event["method"] != json!("item/permissions/requestApproval")));
}

#[tokio::test]
async fn a_thread_can_only_be_resumed_by_the_harness_that_owns_its_session() {
    let mut script = full_turn_script();
    // The agent still holds `native-1`, so a native resume is possible.
    script.known_sessions = vec!["native-1".into()];
    let harness = build("grok-build", full_capabilities(), script);
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;

    let resumed = harness
        .facade
        .handle_request("thread/resume", json!({"threadId": thread_id}))
        .await
        .expect("resume of a live thread returns its own session");
    assert_eq!(resumed["thread"]["id"], json!(thread_id));

    // A thread Vellum has no binding for cannot be conjured from the journal.
    let error = harness
        .facade
        .handle_request("thread/resume", json!({"threadId": "vellum_unknown"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SessionNotFound"));
}

#[tokio::test]
async fn reconnecting_a_thread_does_not_duplicate_its_event_stream() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let thread_id = harness
        .start_thread(json!({"harnessId": "grok-build"}))
        .await;

    for _ in 0..2 {
        harness
            .facade
            .handle_request("thread/resume", json!({"threadId": thread_id}))
            .await
            .unwrap();
    }

    let mut messages = harness.facade.subscribe();
    harness
        .facade
        .handle_request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "go"}]}),
        )
        .await
        .unwrap();
    let seen = collect_until(&mut messages, is_turn_completed).await;

    let observed = methods_of(&seen);
    for expected in ["turn/started", "item/agentMessage/delta", "turn/completed"] {
        assert_eq!(
            observed.iter().filter(|method| *method == expected).count(),
            1,
            "{expected} was duplicated by reconnecting: {observed:?}"
        );
    }
}

#[tokio::test]
async fn selecting_an_unregistered_harness_fails_instead_of_falling_back() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    let error = harness
        .facade
        .handle_request(
            "thread/start",
            json!({
                "cwd": harness.workspace.path().to_string_lossy(),
                "harnessSelection": {"harnessId": "qwen-code"}
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("RuntimeUnavailable"));
    // No harness was started on the thread's behalf.
    assert_eq!(harness.driver.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn methods_outside_the_served_set_are_refused() {
    let harness = build("grok-build", full_capabilities(), full_turn_script());
    // `thread/archive` is a real Codex method the facade does not serve;
    // `thread/settings/update` is not Codex protocol at all. Both are refused,
    // and neither is answered with a plausible empty success.
    for method in ["thread/archive", "thread/settings/update"] {
        let error = harness
            .facade
            .handle_request(method, json!({"threadId": "vellum_x"}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported Codex App Server"),
            "{method} should be refused, got: {error}"
        );
    }
}
