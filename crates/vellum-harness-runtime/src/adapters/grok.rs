use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessId, HarnessTransportKind, ProcessScope,
};

use super::acp::AcpHarnessRuntime;
use crate::process::HarnessLaunchSpec;
use crate::{
    HarnessDriver, HarnessError, HarnessProbeContext, HarnessProbeResult, HarnessRuntime,
    HarnessStartSpec, NativeHarnessSupervisor,
};

/// Native Grok Build adapter. It launches the official `grok agent stdio`
/// process; Vellum never routes a Grok-native thread through its model proxy.
pub struct GrokBuildDriver {
    descriptor: HarnessDescriptor,
    executable: PathBuf,
    supervisor: Arc<NativeHarnessSupervisor>,
}
impl GrokBuildDriver {
    pub fn new(executable: PathBuf, supervisor: Arc<NativeHarnessSupervisor>) -> Self {
        Self {
            executable,
            supervisor,
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
        }
    }
    fn launch_spec(&self, cwd: PathBuf) -> HarnessLaunchSpec {
        HarnessLaunchSpec {
            executable: self.executable.clone(),
            args: vec!["agent".into(), "stdio".into(), "--no-leader".into()],
            cwd,
            env: BTreeMap::new(),
            inherit_env: true,
        }
    }
}
#[async_trait]
impl HarnessDriver for GrokBuildDriver {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    async fn probe(
        &self,
        context: &HarnessProbeContext,
    ) -> Result<HarnessProbeResult, HarnessError> {
        super::probe_by_handshake(self, &self.executable, context).await
    }
    async fn start(&self, spec: HarnessStartSpec) -> Result<Arc<dyn HarnessRuntime>, HarnessError> {
        let process = self
            .supervisor
            .spawn(self.launch_spec(spec.workspace))
            .await?;
        Ok(AcpHarnessRuntime::from_process(self.descriptor.clone(), process).await?)
    }
}
