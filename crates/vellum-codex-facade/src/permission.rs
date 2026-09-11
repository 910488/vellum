//! Permission mediation between the Codex UI and the owning native harness.
//!
//! Correlation is by identifier, never by matching text. A binding is consumed
//! by its first resolution, so a duplicated or replayed approval fails closed
//! instead of reaching the native harness as a second decision.

use std::collections::HashMap;

use tokio::sync::Mutex;
use vellum_harness_protocol::{HarnessId, PermissionBinding};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PermissionError {
    #[error("permission request {0} is unknown or already resolved")]
    AlreadyResolved(String),
}

#[derive(Default)]
pub struct PermissionRegistry {
    bindings: Mutex<HashMap<String, PermissionBinding>>,
}

impl PermissionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a native request and returns the id the UI will answer with.
    pub async fn register(
        &self,
        thread_id: &str,
        harness_id: HarnessId,
        native_request_id: &str,
    ) -> String {
        let ui_request_id = format!("perm_{}", uuid::Uuid::new_v4());
        self.bindings.lock().await.insert(
            ui_request_id.clone(),
            PermissionBinding {
                ui_request_id: ui_request_id.clone(),
                native_request_id: native_request_id.to_owned(),
                thread_id: thread_id.to_owned(),
                harness_id,
            },
        );
        ui_request_id
    }

    /// Consumes the binding. A second call for the same id is an error.
    pub async fn resolve(&self, ui_request_id: &str) -> Result<PermissionBinding, PermissionError> {
        self.bindings
            .lock()
            .await
            .remove(ui_request_id)
            .ok_or_else(|| PermissionError::AlreadyResolved(ui_request_id.to_owned()))
    }

    pub async fn pending(&self) -> usize {
        self.bindings.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_permission_can_only_be_resolved_once() {
        let registry = PermissionRegistry::new();
        let ui_request_id = registry
            .register("ui-1", HarnessId("grok-build".into()), "call-1")
            .await;
        let binding = registry.resolve(&ui_request_id).await.unwrap();
        assert_eq!(binding.native_request_id, "call-1");
        assert_eq!(binding.thread_id, "ui-1");
        assert_eq!(
            registry.resolve(&ui_request_id).await.unwrap_err(),
            PermissionError::AlreadyResolved(ui_request_id)
        );
        assert_eq!(registry.pending().await, 0);
    }

    #[tokio::test]
    async fn bindings_from_different_threads_never_collide() {
        let registry = PermissionRegistry::new();
        let harness = HarnessId("grok-build".into());
        let a = registry.register("ui-a", harness.clone(), "call-1").await;
        let b = registry.register("ui-b", harness, "call-1").await;
        assert_ne!(a, b);
        assert_eq!(registry.resolve(&a).await.unwrap().thread_id, "ui-a");
        assert_eq!(registry.resolve(&b).await.unwrap().thread_id, "ui-b");
    }
}
