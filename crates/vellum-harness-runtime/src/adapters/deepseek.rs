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

/// DeepSeek Harness ACP. Capability false values deliberately prevent the
/// facade from inventing plans, terminal, transcript replay, or subagents.
pub struct DeepSeekHarnessDriver {
    descriptor: HarnessDescriptor,
    executable: PathBuf,
    supervisor: Arc<NativeHarnessSupervisor>,
}
impl DeepSeekHarnessDriver {
    pub fn new(executable: PathBuf, supervisor: Arc<NativeHarnessSupervisor>) -> Self {
        Self {
            executable,
            supervisor,
            descriptor: HarnessDescriptor {
                id: HarnessId(HarnessId::DEEPSEEK_HARNESS.into()),
                display_name: "DeepSeek Harness".into(),
                vendor: "DeepSeek".into(),
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
                    compaction: false,
                    plans: false,
                    commands: false,
                    terminals: false,
                    subagents: false,
                    native_memory: false,
                    usage: true,
                    context_usage: true,
                },
            },
        }
    }
    fn launch(&self, cwd: PathBuf) -> HarnessLaunchSpec {
        HarnessLaunchSpec {
            executable: self.executable.clone(),
            args: vec!["dsh".into(), "--profile".into(), "acp".into()],
            cwd,
            env: BTreeMap::new(),
            inherit_env: true,
        }
    }
}
#[async_trait]
impl HarnessDriver for DeepSeekHarnessDriver {
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
        Ok(AcpHarnessRuntime::from_process(
            self.descriptor.clone(),
            self.supervisor.spawn(self.launch(spec.workspace)).await?,
        )
        .await?)
    }
}
