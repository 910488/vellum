use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessErrorCategory, HarnessEventEnvelope, HarnessId,
    HarnessSessionHandle,
};

#[derive(Debug, Clone)]
pub struct HarnessProbeContext {
    pub workspace: PathBuf,
}
#[derive(Debug, Clone)]
pub struct HarnessProbeResult {
    pub harness_id: HarnessId,
    pub available: bool,
    pub capabilities: Option<HarnessCapabilities>,
    pub diagnostic: Option<String>,
}
#[derive(Debug, Clone)]
pub struct HarnessStartSpec {
    pub workspace: PathBuf,
}
#[derive(Debug, Clone)]
pub struct CreateSessionSpec {
    pub thread_id: String,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}
#[derive(Debug, Clone)]
pub struct ResumeSessionSpec {
    pub thread_id: String,
    pub native_session_id: String,
    pub workspace: PathBuf,
}
#[derive(Debug, Clone)]
pub struct SessionListQuery {
    pub workspace: Option<PathBuf>,
}
#[derive(Debug, Clone)]
pub struct NativeSessionSummary {
    pub native_session_id: String,
    pub title: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub text: String,
    pub metadata: Value,
}
#[derive(Debug, Clone)]
pub struct PromptHandle {
    pub native_turn_id: Option<String>,
}
#[derive(Debug, Clone)]
pub struct CancelRequest {
    pub native_turn_id: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PermissionResolution {
    pub native_request_id: String,
    pub granted: bool,
    pub payload: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("{category:?}: {message}")]
    Categorized {
        category: HarnessErrorCategory,
        message: String,
    },
}
impl HarnessError {
    pub fn unsupported(capability: &str) -> Self {
        Self::Categorized {
            category: HarnessErrorCategory::UnsupportedCapability,
            message: capability.into(),
        }
    }
}

#[async_trait]
pub trait HarnessDriver: Send + Sync {
    fn descriptor(&self) -> &HarnessDescriptor;
    async fn probe(
        &self,
        context: &HarnessProbeContext,
    ) -> Result<HarnessProbeResult, HarnessError>;
    async fn start(&self, spec: HarnessStartSpec) -> Result<Arc<dyn HarnessRuntime>, HarnessError>;
}
#[async_trait]
pub trait HarnessRuntime: Send + Sync {
    fn descriptor(&self) -> &HarnessDescriptor;
    async fn create_session(
        &self,
        spec: CreateSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError>;
    async fn resume_session(
        &self,
        spec: ResumeSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError>;
    async fn list_sessions(
        &self,
        query: SessionListQuery,
    ) -> Result<Vec<NativeSessionSummary>, HarnessError>;
    async fn close_session(&self, session: &HarnessSessionHandle) -> Result<(), HarnessError>;
    /// Returns a live session owned by this runtime. A handle from another
    /// runtime instance is never recoverable through a transcript replay.
    async fn session(
        &self,
        session: &HarnessSessionHandle,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError>;
    async fn shutdown(&self) -> Result<(), HarnessError>;
}
#[async_trait]
pub trait HarnessSession: Send + Sync {
    fn handle(&self) -> &HarnessSessionHandle;
    fn capabilities(&self) -> &HarnessCapabilities;
    async fn prompt(&self, request: PromptRequest) -> Result<PromptHandle, HarnessError>;
    async fn cancel(&self, request: CancelRequest) -> Result<(), HarnessError>;
    async fn resolve_permission(&self, response: PermissionResolution) -> Result<(), HarnessError>;
    async fn set_model(&self, model: String) -> Result<(), HarnessError>;
    async fn set_reasoning_effort(&self, effort: String) -> Result<(), HarnessError>;
    async fn compact(&self) -> Result<(), HarnessError>;
    fn subscribe(&self) -> broadcast::Receiver<HarnessEventEnvelope>;
}

#[derive(Default)]
pub struct HarnessRegistry {
    drivers: HashMap<HarnessId, Arc<dyn HarnessDriver>>,
}
impl HarnessRegistry {
    pub fn register(&mut self, driver: Arc<dyn HarnessDriver>) -> Result<(), HarnessRegistryError> {
        let id = driver.descriptor().id.clone();
        if self.drivers.insert(id.clone(), driver).is_some() {
            return Err(HarnessRegistryError::Duplicate(id));
        }
        Ok(())
    }
    pub fn get(&self, id: &HarnessId) -> Option<Arc<dyn HarnessDriver>> {
        self.drivers.get(id).cloned()
    }
    pub async fn probe_all(&self, context: &HarnessProbeContext) -> Vec<HarnessProbeResult> {
        let mut values = Vec::new();
        for driver in self.drivers.values() {
            values.push(
                driver
                    .probe(context)
                    .await
                    .unwrap_or_else(|error| HarnessProbeResult {
                        harness_id: driver.descriptor().id.clone(),
                        available: false,
                        capabilities: None,
                        diagnostic: Some(error.to_string()),
                    }),
            );
        }
        values
    }
}
#[derive(Debug, thiserror::Error)]
pub enum HarnessRegistryError {
    #[error("duplicate harness id: {0:?}")]
    Duplicate(HarnessId),
}
