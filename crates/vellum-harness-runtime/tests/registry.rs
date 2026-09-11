//! Registry behaviour (§44 of the harness manager plan).
//!
//! The registry is what the UI sees instead of executable paths. Its job is to
//! report, truthfully, which harnesses exist and what each one can do — and to
//! keep a failed probe distinct from a failed runtime.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessErrorCategory, HarnessId, HarnessTransportKind,
    ProcessScope,
};
use vellum_harness_runtime::{
    HarnessDriver, HarnessError, HarnessProbeContext, HarnessProbeResult, HarnessRegistry,
    HarnessRegistryError, HarnessRuntime, HarnessStartSpec,
};

struct StubDriver {
    descriptor: HarnessDescriptor,
    available: bool,
    probes: AtomicUsize,
}

impl StubDriver {
    fn new(id: &str, available: bool, capabilities: HarnessCapabilities) -> Self {
        Self {
            available,
            probes: AtomicUsize::new(0),
            descriptor: HarnessDescriptor {
                id: HarnessId(id.into()),
                display_name: id.into(),
                vendor: "test".into(),
                transport: HarnessTransportKind::AcpStdio,
                process_scope: ProcessScope::PerWorkspace,
                capabilities,
            },
        }
    }
}

#[async_trait]
impl HarnessDriver for StubDriver {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    async fn probe(&self, _: &HarnessProbeContext) -> Result<HarnessProbeResult, HarnessError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        if !self.available {
            return Err(HarnessError::Categorized {
                category: HarnessErrorCategory::RuntimeUnavailable,
                message: "executable not found on PATH".into(),
            });
        }
        Ok(HarnessProbeResult {
            harness_id: self.descriptor.id.clone(),
            available: true,
            capabilities: Some(self.descriptor.capabilities.clone()),
            diagnostic: None,
        })
    }
    async fn start(&self, _: HarnessStartSpec) -> Result<Arc<dyn HarnessRuntime>, HarnessError> {
        Err(HarnessError::Categorized {
            category: HarnessErrorCategory::Internal,
            message: "stub driver does not start".into(),
        })
    }
}

fn context() -> HarnessProbeContext {
    HarnessProbeContext {
        workspace: PathBuf::from("."),
    }
}

#[tokio::test]
async fn a_duplicate_harness_id_is_rejected_rather_than_shadowing_the_first() {
    let mut registry = HarnessRegistry::default();
    registry
        .register(Arc::new(StubDriver::new(
            "grok-build",
            true,
            HarnessCapabilities::default(),
        )))
        .unwrap();
    let error = registry
        .register(Arc::new(StubDriver::new(
            "grok-build",
            true,
            HarnessCapabilities::default(),
        )))
        .unwrap_err();
    assert!(matches!(
        error,
        HarnessRegistryError::Duplicate(id) if id.0 == "grok-build"
    ));
}

#[tokio::test]
async fn a_harness_that_was_never_registered_is_simply_absent() {
    let registry = HarnessRegistry::default();
    assert!(registry
        .get(&HarnessId("deepseek-harness".into()))
        .is_none());
}

#[tokio::test]
async fn a_probe_failure_is_reported_as_unavailable_not_propagated_as_a_runtime_failure() {
    let mut registry = HarnessRegistry::default();
    registry
        .register(Arc::new(StubDriver::new(
            "grok-build",
            true,
            HarnessCapabilities {
                compaction: true,
                ..Default::default()
            },
        )))
        .unwrap();
    registry
        .register(Arc::new(StubDriver::new(
            "deepseek-harness",
            false,
            HarnessCapabilities::default(),
        )))
        .unwrap();

    let mut results = registry.probe_all(&context()).await;
    results.sort_by(|a, b| a.harness_id.0.cmp(&b.harness_id.0));
    assert_eq!(results.len(), 2);

    let deepseek = &results[0];
    assert_eq!(deepseek.harness_id.0, "deepseek-harness");
    assert!(!deepseek.available);
    assert!(deepseek.capabilities.is_none());
    assert!(deepseek
        .diagnostic
        .as_ref()
        .is_some_and(|detail| detail.contains("RuntimeUnavailable")));

    // A driver that cannot be probed is still registered: the harness exists,
    // it just is not usable right now.
    assert!(registry
        .get(&HarnessId("deepseek-harness".into()))
        .is_some());

    let grok = &results[1];
    assert!(grok.available);
    assert!(grok.capabilities.as_ref().unwrap().compaction);
}

#[tokio::test]
async fn capabilities_are_taken_from_the_probe_not_assumed_by_the_caller() {
    let mut registry = HarnessRegistry::default();
    registry
        .register(Arc::new(StubDriver::new(
            "deepseek-harness",
            true,
            HarnessCapabilities {
                session_resume: true,
                permissions: true,
                // DeepSeek's ACP exposes none of these, and the registry must
                // not round them up on the UI's behalf.
                plans: false,
                commands: false,
                terminals: false,
                subagents: false,
                ..Default::default()
            },
        )))
        .unwrap();
    let results = registry.probe_all(&context()).await;
    let capabilities = results[0].capabilities.as_ref().unwrap();
    assert!(capabilities.session_resume);
    assert!(!capabilities.plans);
    assert!(!capabilities.commands);
    assert!(!capabilities.terminals);
    assert!(!capabilities.subagents);
}

#[tokio::test]
async fn probing_is_performed_per_call_so_a_stale_result_cannot_be_served_forever() {
    let driver = Arc::new(StubDriver::new(
        "qwen-code",
        true,
        HarnessCapabilities::default(),
    ));
    let mut registry = HarnessRegistry::default();
    registry.register(driver.clone()).unwrap();
    registry.probe_all(&context()).await;
    registry.probe_all(&context()).await;
    assert_eq!(driver.probes.load(Ordering::SeqCst), 2);
}

#[test]
fn an_uninstalled_harness_is_detected_without_spawning_anything() {
    // A bare name is resolved against PATH; a path with directories is checked
    // directly. Neither case may report a harness the user does not have.
    assert!(!vellum_harness_runtime::executable_is_available(
        &PathBuf::from("vellum-definitely-not-installed-harness")
    ));
    assert!(!vellum_harness_runtime::executable_is_available(
        &PathBuf::from("./no/such/harness")
    ));
    // The current test binary is a real file, so the direct-path branch has a
    // positive case too.
    assert!(vellum_harness_runtime::executable_is_available(
        &std::env::current_exe().unwrap()
    ));
}

#[tokio::test]
async fn a_driver_whose_executable_is_missing_reports_unavailable_not_an_error() {
    use vellum_harness_runtime::{adapters::grok::GrokBuildDriver, NativeHarnessSupervisor};

    let workspace = tempfile::tempdir().unwrap();
    let driver = GrokBuildDriver::new(
        PathBuf::from("vellum-definitely-not-installed-harness"),
        Arc::new(NativeHarnessSupervisor::default()),
    );
    let result = driver
        .probe(&HarnessProbeContext {
            workspace: workspace.path().into(),
        })
        .await
        .expect("a missing harness is a probe result, not a probe failure");
    assert!(!result.available);
    assert!(result.capabilities.is_none());
    assert!(result
        .diagnostic
        .as_ref()
        .is_some_and(|detail| detail.contains("was not found")));
}
