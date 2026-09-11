pub mod acp;
pub mod deepseek;
pub mod grok;
pub mod qwen;
pub mod zcode_desktop;

use std::path::Path;

use crate::{
    HarnessDriver, HarnessError, HarnessProbeContext, HarnessProbeResult, HarnessStartSpec,
};

/// Shared probe for the ACP stdio adapters.
///
/// Availability is established by a real handshake, because an installed
/// binary that cannot complete `initialize` is not a usable harness. A missing
/// executable short-circuits before any process is spawned, and neither case
/// is reported as a runtime failure: the harness is simply unavailable, and
/// the diagnostic says why.
pub(crate) async fn probe_by_handshake(
    driver: &(impl HarnessDriver + ?Sized),
    executable: &Path,
    context: &HarnessProbeContext,
) -> Result<HarnessProbeResult, HarnessError> {
    let descriptor = driver.descriptor();
    let unavailable = |diagnostic: String| HarnessProbeResult {
        harness_id: descriptor.id.clone(),
        available: false,
        capabilities: None,
        diagnostic: Some(diagnostic),
    };

    if !crate::executable_is_available(executable) {
        return Ok(unavailable(format!(
            "{} was not found",
            executable.display()
        )));
    }
    if !context.workspace.is_dir() {
        return Ok(unavailable(format!(
            "workspace does not exist: {}",
            context.workspace.display()
        )));
    }

    let runtime = match driver
        .start(HarnessStartSpec {
            workspace: context.workspace.clone(),
        })
        .await
    {
        Ok(runtime) => runtime,
        Err(error) => return Ok(unavailable(error.to_string())),
    };
    let capabilities = runtime.descriptor().capabilities.clone();
    let shutdown = runtime.shutdown().await;
    Ok(HarnessProbeResult {
        harness_id: descriptor.id.clone(),
        available: true,
        capabilities: Some(capabilities),
        diagnostic: shutdown.err().map(|error| error.to_string()),
    })
}
