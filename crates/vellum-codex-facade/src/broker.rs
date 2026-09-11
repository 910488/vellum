//! Harness selection and session ownership.
//!
//! The broker binds a UI thread to exactly one native session and keeps that
//! binding for the life of the thread. Switching harness means a new thread;
//! there is no silent migration, because native memory, tools, plans,
//! subagents and compaction are not interchangeable between harnesses.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use sha2::Digest;
use tokio::sync::Mutex;
use vellum_harness_protocol::{
    CompactionAuthority, HarnessErrorCategory, HarnessId, HarnessSelection, HarnessSessionBinding,
};
use vellum_harness_runtime::{
    CreateSessionSpec, HarnessBindingStore, HarnessError, HarnessRegistry, HarnessRuntime,
    HarnessSession, HarnessStartSpec, NativeSessionSummary, ResumeSessionSpec, SessionListQuery,
};

/// Which subsystem owns compaction for a harness. Crossing this boundary is
/// what makes a benchmark result void, so it is decided once, by identity.
pub fn compaction_authority(id: &HarnessId) -> CompactionAuthority {
    match id.0.as_str() {
        HarnessId::CODEX => CompactionAuthority::CodexNative,
        HarnessId::VELLUM_GENERIC => CompactionAuthority::VellumGeneric,
        // DeepSeek's ACP does not expose compaction; declaring it unsupported
        // is what stops the facade from substituting another engine.
        HarnessId::DEEPSEEK_HARNESS => CompactionAuthority::Unsupported,
        _ => CompactionAuthority::NativeHarness,
    }
}

#[async_trait]
pub trait HarnessBroker: Send + Sync {
    async fn create_session(
        &self,
        selection: HarnessSelection,
        spec: CreateSessionSpec,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError>;

    /// Asks the owning native runtime to restore a session. It must fail when
    /// that runtime cannot restore it; Vellum has no second way to produce one.
    async fn resume_session(
        &self,
        thread_id: &str,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError>;

    async fn session_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError>;

    async fn list_native_sessions(
        &self,
        thread_id: &str,
    ) -> Result<Vec<NativeSessionSummary>, HarnessError>;

    async fn binding_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<HarnessSessionBinding, HarnessError>;

    async fn compaction_authority(
        &self,
        thread_id: &str,
    ) -> Result<CompactionAuthority, HarnessError>;
}

/// Production broker. Native sessions live in process memory; only their
/// bindings are persisted, and a binding is an index, never a transcript.
pub struct ManagedHarnessBroker {
    registry: Arc<HarnessRegistry>,
    bindings: Arc<HarnessBindingStore>,
    sessions: Mutex<HashMap<String, Arc<dyn HarnessSession>>>,
    runtimes: Mutex<HashMap<String, Arc<dyn HarnessRuntime>>>,
}

impl ManagedHarnessBroker {
    pub fn new(registry: Arc<HarnessRegistry>, bindings: Arc<HarnessBindingStore>) -> Self {
        Self {
            registry,
            bindings,
            sessions: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(HashMap::new()),
        }
    }

    async fn runtime_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Arc<dyn HarnessRuntime>, HarnessError> {
        self.runtimes
            .lock()
            .await
            .get(thread_id)
            .cloned()
            .ok_or_else(|| session_not_found(thread_id))
    }
}

#[async_trait]
impl HarnessBroker for ManagedHarnessBroker {
    async fn create_session(
        &self,
        selection: HarnessSelection,
        spec: CreateSessionSpec,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
        let driver =
            self.registry
                .get(&selection.harness_id)
                .ok_or_else(|| HarnessError::Categorized {
                    category: HarnessErrorCategory::RuntimeUnavailable,
                    message: format!("selected harness unavailable: {}", selection.harness_id.0),
                })?;
        let runtime = driver
            .start(HarnessStartSpec {
                workspace: spec.workspace.clone(),
            })
            .await?;
        let handle = runtime
            .create_session(CreateSessionSpec {
                thread_id: spec.thread_id.clone(),
                workspace: spec.workspace.clone(),
                model: selection.model_id.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
            })
            .await?;
        let session = runtime.session(&handle).await?;
        let now = Utc::now();
        self.bindings
            .upsert(&HarnessSessionBinding {
                ui_thread_id: spec.thread_id.clone(),
                harness_id: selection.harness_id,
                runtime_instance_id: handle.runtime_instance_id,
                native_session_id: handle.native_session_id,
                workspace: spec.workspace,
                capability_snapshot_hash: capability_hash(session.as_ref())?,
                created_at: now,
                last_seen_at: now,
            })
            .map_err(internal)?;
        self.runtimes
            .lock()
            .await
            .insert(spec.thread_id.clone(), runtime);
        self.sessions
            .lock()
            .await
            .insert(spec.thread_id, session.clone());
        Ok(session)
    }

    async fn resume_session(
        &self,
        thread_id: &str,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
        if let Some(session) = self.sessions.lock().await.get(thread_id) {
            return Ok(session.clone());
        }
        let binding = self.binding_for_thread(thread_id).await?;
        let driver =
            self.registry
                .get(&binding.harness_id)
                .ok_or_else(|| HarnessError::Categorized {
                    category: HarnessErrorCategory::RuntimeUnavailable,
                    message: format!("selected harness unavailable: {}", binding.harness_id.0),
                })?;
        if !driver.descriptor().capabilities.session_resume {
            // The binding stays; the thread simply has no live session until
            // the user starts a new one. Replaying UI history into a fresh
            // native session would corrupt that harness's own state.
            return Err(HarnessError::unsupported("sessionResume"));
        }
        let runtime = driver
            .start(HarnessStartSpec {
                workspace: binding.workspace.clone(),
            })
            .await?;
        let handle = runtime
            .resume_session(ResumeSessionSpec {
                thread_id: thread_id.to_owned(),
                native_session_id: binding.native_session_id.clone(),
                workspace: binding.workspace.clone(),
            })
            .await?;
        let session = runtime.session(&handle).await?;
        self.bindings
            .touch(thread_id, Utc::now())
            .map_err(internal)?;
        self.runtimes
            .lock()
            .await
            .insert(thread_id.to_owned(), runtime);
        self.sessions
            .lock()
            .await
            .insert(thread_id.to_owned(), session.clone());
        Ok(session)
    }

    async fn session_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
        self.sessions
            .lock()
            .await
            .get(thread_id)
            .cloned()
            .ok_or_else(|| session_not_found(thread_id))
    }

    async fn list_native_sessions(
        &self,
        thread_id: &str,
    ) -> Result<Vec<NativeSessionSummary>, HarnessError> {
        let runtime = self.runtime_for_thread(thread_id).await?;
        if !runtime.descriptor().capabilities.session_list {
            return Err(HarnessError::unsupported("sessionList"));
        }
        let binding = self.binding_for_thread(thread_id).await?;
        runtime
            .list_sessions(SessionListQuery {
                workspace: Some(binding.workspace),
            })
            .await
    }

    async fn binding_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<HarnessSessionBinding, HarnessError> {
        self.bindings
            .get(thread_id)
            .map_err(internal)?
            .ok_or_else(|| session_not_found(thread_id))
    }

    async fn compaction_authority(
        &self,
        thread_id: &str,
    ) -> Result<CompactionAuthority, HarnessError> {
        Ok(compaction_authority(
            &self.binding_for_thread(thread_id).await?.harness_id,
        ))
    }
}

fn capability_hash(session: &dyn HarnessSession) -> Result<String, HarnessError> {
    let encoded = serde_json::to_vec(session.capabilities()).map_err(internal)?;
    Ok(format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(encoded))
    ))
}

fn session_not_found(thread_id: &str) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::SessionNotFound,
        message: format!("no native session bound to UI thread {thread_id}"),
    }
}

fn internal(error: impl std::fmt::Display) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::Internal,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_has_exactly_one_compaction_owner() {
        assert_eq!(
            compaction_authority(&HarnessId(HarnessId::CODEX.into())),
            CompactionAuthority::CodexNative
        );
        assert_eq!(
            compaction_authority(&HarnessId(HarnessId::VELLUM_GENERIC.into())),
            CompactionAuthority::VellumGeneric
        );
        assert_eq!(
            compaction_authority(&HarnessId(HarnessId::DEEPSEEK_HARNESS.into())),
            CompactionAuthority::Unsupported
        );
        for native in [HarnessId::GROK_BUILD, HarnessId::QWEN_CODE] {
            assert_eq!(
                compaction_authority(&HarnessId(native.into())),
                CompactionAuthority::NativeHarness
            );
        }
    }
}
