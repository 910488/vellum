//! Upstream version/schema compatibility gate.

use crate::app_server::transport::{AppServerError, ServerIdentity};
use crate::config::BrokerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityState {
    Compatible,
    Incompatible,
}

pub fn evaluate(
    config: &BrokerConfig,
    identity: &ServerIdentity,
) -> Result<CompatibilityState, AppServerError> {
    if config.allowed_versions.is_empty() {
        return Ok(CompatibilityState::Compatible);
    }
    if config
        .allowed_versions
        .iter()
        .any(|allowed| identity.version.starts_with(allowed) || allowed == &identity.version)
    {
        Ok(CompatibilityState::Compatible)
    } else {
        Err(AppServerError::Incompatible(format!(
            "codex version {} is not in allowed set {:?}",
            identity.version, config.allowed_versions
        )))
    }
}
