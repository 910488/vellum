//! Bootstrap replay (§49 of the harness manager plan).
//!
//! Drives `evals/fixtures/codex-ui/bootstrap-jsonrpc.jsonl` through the facade
//! against a scripted agent. The fixture's method names are schema-verified
//! elsewhere; this test proves the facade actually answers them in sequence,
//! with the thread id threaded through as a real client would.
//!
//! This is a surface replay, not a captured session — see that fixture's
//! README for what is still outstanding.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use vellum_codex_facade::{CodexAppServerFacade, ManagedHarnessBroker};
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessId, HarnessTransportKind, ProcessScope,
};
use vellum_harness_runtime::{
    adapters::acp::AcpHarnessRuntime, HarnessBindingStore, HarnessDriver, HarnessError,
    HarnessLaunchSpec, HarnessProbeContext, HarnessProbeResult, HarnessRegistry, HarnessRuntime,
    HarnessStartSpec, NativeHarnessSupervisor,
};
use vellum_harness_testkit::{FakeAgentScript, ScriptedEvent};

struct ReplayDriver {
    descriptor: HarnessDescriptor,
    script_path: PathBuf,
    supervisor: Arc<NativeHarnessSupervisor>,
}

#[async_trait]
impl HarnessDriver for ReplayDriver {
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

fn driver(script_dir: &Path) -> Arc<ReplayDriver> {
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
    let script_path = script_dir.join("replay-script.json");
    std::fs::write(&script_path, serde_json::to_string(&script).unwrap()).unwrap();
    Arc::new(ReplayDriver {
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
                usage: true,
                context_usage: true,
                ..Default::default()
            },
        },
        script_path,
        supervisor: Arc::new(NativeHarnessSupervisor::default()),
    })
}

fn fixture() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("evals/fixtures/codex-ui/bootstrap-jsonrpc.jsonl");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("bootstrap fixture {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture line is valid JSON-RPC"))
        .collect()
}

#[tokio::test]
async fn the_pinned_bootstrap_sequence_is_answered_end_to_end() {
    let workspace = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let mut registry = HarnessRegistry::default();
    registry.register(driver(scripts.path())).unwrap();
    let bindings =
        Arc::new(HarnessBindingStore::open(scripts.path().join("bindings.sqlite")).unwrap());
    let facade = CodexAppServerFacade::new(Arc::new(ManagedHarnessBroker::new(
        Arc::new(registry),
        bindings,
    )));

    let mut thread_id: Option<String> = None;
    let mut answered = Vec::new();

    for frame in fixture() {
        let method = frame["method"]
            .as_str()
            .expect("fixture frame has a method");
        let mut params = frame["params"].clone();

        // The fixture uses `$THREAD` where a real client would echo the id it
        // received from `thread/start`.
        if params["threadId"] == json!("$THREAD") {
            let id = thread_id
                .clone()
                .expect("a thread must be started before it is used");
            params["threadId"] = json!(id);
        }
        // The fixture's cwd is a placeholder; the workspace must really exist.
        if params.get("cwd").is_some() {
            params["cwd"] = json!(workspace.path().to_string_lossy());
        }

        let response = facade
            .handle_request(method, params)
            .await
            .unwrap_or_else(|error| panic!("bootstrap `{method}` was not answered: {error}"));

        if method == "thread/start" {
            thread_id = Some(
                response["thread"]["id"]
                    .as_str()
                    .expect("thread/start returns a thread id")
                    .to_owned(),
            );
        }
        answered.push(method.to_owned());
    }

    // Every frame in the fixture was answered, in order.
    assert_eq!(
        answered,
        vec![
            "initialize",
            "model/list",
            "thread/start",
            "config/value/write",
            "turn/start",
            "thread/read",
            "turn/interrupt",
            "thread/compact/start",
        ]
    );
    assert!(thread_id.is_some());
}

#[tokio::test]
async fn the_fixture_only_uses_methods_the_facade_actually_serves() {
    use vellum_codex_facade::methods::{classify, MethodClass};
    for frame in fixture() {
        let method = frame["method"].as_str().unwrap();
        assert_ne!(
            classify(method),
            MethodClass::Unsupported,
            "the bootstrap fixture uses `{method}`, which the facade refuses"
        );
    }
}
