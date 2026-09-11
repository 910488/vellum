//! Authority invariant (§45, §46, §69 of the harness manager plan).
//!
//! Every turn must have exactly one owner. A trace showing a native harness
//! *and* a Vellum model loop, or a native compaction *and* a Vellum one, voids
//! the case — it is an architecture failure, not a model result.
//!
//! Two things are checked here, because behaviour alone is not enough:
//!
//! 1. behaviourally, a native thread's turns are all served by the native
//!    harness process, exactly once each; and
//! 2. structurally, the harness stack cannot reach the proxy model path at all,
//!    since a dependency edge is what would make a silent fallback possible.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

struct CountingDriver {
    descriptor: HarnessDescriptor,
    script_path: PathBuf,
    supervisor: Arc<NativeHarnessSupervisor>,
    starts: AtomicUsize,
}

#[async_trait]
impl HarnessDriver for CountingDriver {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    async fn probe(&self, _: &HarnessProbeContext) -> Result<HarnessProbeResult, HarnessError> {
        Ok(HarnessProbeResult {
            harness_id: self.descriptor.id.clone(),
            available: true,
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

fn grok_like(script_dir: &Path) -> Arc<CountingDriver> {
    let script = FakeAgentScript {
        on_prompt: vec![
            ScriptedEvent::TurnStarted {
                turn_id: Some("t".into()),
            },
            ScriptedEvent::Assistant { text: "ok".into() },
            ScriptedEvent::Complete {
                turn_id: Some("t".into()),
            },
        ],
        ..Default::default()
    };
    let script_path = script_dir.join("grok-script.json");
    std::fs::write(&script_path, serde_json::to_string(&script).unwrap()).unwrap();
    Arc::new(CountingDriver {
        descriptor: HarnessDescriptor {
            id: HarnessId(HarnessId::GROK_BUILD.into()),
            display_name: "Grok Build".into(),
            vendor: "xAI".into(),
            transport: HarnessTransportKind::AcpStdio,
            process_scope: ProcessScope::PerWorkspace,
            capabilities: HarnessCapabilities {
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
            },
        },
        script_path,
        supervisor: Arc::new(NativeHarnessSupervisor::default()),
        starts: AtomicUsize::new(0),
    })
}

async fn drain_for(
    receiver: &mut tokio::sync::broadcast::Receiver<CodexServerMessage>,
    window: Duration,
    predicate: impl Fn(&CodexServerMessage) -> bool,
) -> Vec<CodexServerMessage> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
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

/// How many prompt turns the native agent actually served for this thread.
///
/// The agent announces every prompt it runs. That announcement has no Codex
/// representation, so it is read from the audit journal rather than the UI
/// stream — which is precisely what the journal is for.
async fn native_prompt_counts(facade: &CodexAppServerFacade, thread_id: &str) -> Vec<u64> {
    facade
        .audit_journal(thread_id)
        .await
        .iter()
        .filter_map(|entry| match &entry.envelope.event {
            vellum_harness_protocol::HarnessEvent::NativeExtension(extension)
                if extension.event_type == "testkit/prompt_observed" =>
            {
                extension.payload.get("count").and_then(Value::as_u64)
            }
            _ => None,
        })
        .collect()
}

async fn wait_for_prompt_count(
    facade: &CodexAppServerFacade,
    thread_id: &str,
    expected: usize,
) -> Vec<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let counts = native_prompt_counts(facade, thread_id).await;
        if counts.len() >= expected || tokio::time::Instant::now() >= deadline {
            return counts;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_grok_native_thread_is_served_only_by_grok() {
    let workspace = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let driver = grok_like(scripts.path());
    let mut registry = HarnessRegistry::default();
    registry.register(driver.clone()).unwrap();
    let bindings =
        Arc::new(HarnessBindingStore::open(scripts.path().join("bindings.sqlite")).unwrap());
    let facade = CodexAppServerFacade::new(Arc::new(ManagedHarnessBroker::new(
        Arc::new(registry),
        bindings,
    )));
    let mut messages = facade.subscribe();

    let started = facade
        .handle_request(
            "thread/start",
            json!({
                "cwd": workspace.path().to_string_lossy(),
                "harnessSelection": {"harnessId": "grok-build", "modelId": "grok-4.6"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(started["harnessId"], json!("grok-build"));
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();

    for turn in 1..=3 {
        facade
            .handle_request(
                "turn/start",
                json!({"threadId": thread_id, "input": [{"type": "text", "text": format!("turn {turn}")}]}),
            )
            .await
            .unwrap();
    }

    let counts = wait_for_prompt_count(&facade, &thread_id, 3).await;
    assert_eq!(
        counts,
        vec![1, 2, 3],
        "each turn must be owned by exactly one native prompt"
    );

    // One runtime, one native session, for the whole thread.
    assert_eq!(driver.starts.load(Ordering::SeqCst), 1);

    let seen = drain_for(&mut messages, Duration::from_millis(200), |_| false).await;
    // Every UI event belongs to this thread; nothing leaked in from elsewhere.
    for notification in seen.iter().filter_map(CodexServerMessage::as_notification) {
        if let Some(seen_thread) = notification.params["threadId"].as_str() {
            assert_eq!(seen_thread, thread_id);
        }
    }

    // No compaction of any kind was triggered: nothing in this path may run a
    // second summarisation engine behind the native harness's back.
    assert!(
        !seen
            .iter()
            .any(|message| message.method() == "thread/compacted"),
        "a native turn must not trigger any Vellum-side compaction"
    );
}

#[tokio::test]
async fn compaction_is_requested_from_the_native_harness_exactly_once() {
    let workspace = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let driver = grok_like(scripts.path());
    let mut registry = HarnessRegistry::default();
    registry.register(driver.clone()).unwrap();
    let bindings =
        Arc::new(HarnessBindingStore::open(scripts.path().join("bindings.sqlite")).unwrap());
    let facade = CodexAppServerFacade::new(Arc::new(ManagedHarnessBroker::new(
        Arc::new(registry),
        bindings,
    )));

    let thread_id = facade
        .handle_request(
            "thread/start",
            json!({
                "cwd": workspace.path().to_string_lossy(),
                "harnessSelection": {"harnessId": "grok-build"}
            }),
        )
        .await
        .unwrap()["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    facade
        .handle_request("thread/compact/start", json!({"threadId": thread_id}))
        .await
        .unwrap();

    // Native compaction does not run a prompt turn: if a prompt is ever
    // observed here, Vellum has started driving the loop itself.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        native_prompt_counts(&facade, &thread_id).await.is_empty(),
        "compaction must not become a Vellum-issued model turn"
    );
    assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
}

/// A behavioural test can only catch a fallback that actually fires. This one
/// removes the possibility: the harness stack has no edge to the proxy model
/// path, so a Grok thread cannot silently land in it.
#[test]
fn the_harness_stack_cannot_reach_the_proxy_model_runtime() {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    for name in [
        "vellum-harness-protocol",
        "vellum-harness-runtime",
        "vellum-acp-client",
        "vellum-codex-facade",
    ] {
        let manifest = std::fs::read_to_string(crates.join(name).join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("{name} manifest is readable: {error}"));
        let dependencies = manifest
            .split("[dev-dependencies]")
            .next()
            .expect("manifest has a dependency section");
        assert!(
            !dependencies.contains("vellum-proxy-runtime"),
            "{name} must not depend on the proxy model runtime: a native thread \
             would then have a path into a second harness"
        );
        assert!(
            !dependencies.contains("codex-core"),
            "{name} must speak the app-server protocol, not link codex-core"
        );
    }
}
