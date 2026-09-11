use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use vellum_acp_client::{AcpClient, AcpIncoming};
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessErrorCategory, HarnessEvent,
    HarnessEventEnvelope, HarnessId, HarnessSessionHandle,
};

use crate::mapper::{AcpEventMapper, EventMapContext, NativeEventMapper};
use crate::process::ManagedHarnessProcess;
use crate::{
    CancelRequest, CreateSessionSpec, HarnessError, HarnessRuntime, HarnessSession,
    NativeSessionSummary, PermissionResolution, PromptHandle, PromptRequest, ResumeSessionSpec,
    SessionListQuery,
};

/// Namespace used to preserve provider-specific events that have no neutral
/// equivalent. Keeping them under a per-vendor namespace means a future mapper
/// can promote them without having to guess where they came from.
fn extension_namespace(id: &HarnessId) -> String {
    match id.0.as_str() {
        HarnessId::GROK_BUILD => "xai",
        HarnessId::QWEN_CODE => "qwen",
        HarnessId::DEEPSEEK_HARNESS => "deepseek",
        _ => "acp",
    }
    .into()
}

pub struct AcpHarnessRuntime {
    descriptor: HarnessDescriptor,
    client: Arc<AcpClient>,
    process: Option<Arc<Mutex<ManagedHarnessProcess>>>,
    instance_id: String,
    sessions: Mutex<HashMap<String, Arc<AcpHarnessSession>>>,
}

impl AcpHarnessRuntime {
    pub async fn from_process(
        descriptor: HarnessDescriptor,
        process: Arc<Mutex<ManagedHarnessProcess>>,
    ) -> Result<Arc<Self>, HarnessError> {
        let (stdin, stdout, instance_id) = {
            let mut process_guard = process.lock().await;
            let stdin = process_guard
                .stdin
                .take()
                .ok_or_else(|| unavailable("ACP stdin already claimed"))?;
            let stdout = process_guard
                .stdout
                .take()
                .ok_or_else(|| unavailable("ACP stdout already claimed"))?;
            (stdin, stdout, process_guard.key.0.clone())
        };
        let client = Arc::new(AcpClient::new(stdout, stdin));
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "vellum", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .map_err(acp_error)?;
        Ok(Arc::new(Self {
            descriptor,
            client,
            process: Some(process),
            instance_id,
            sessions: Mutex::new(HashMap::new()),
        }))
    }

    /// Binds a runtime to an already-connected ACP transport. Contract tests
    /// use it to drive a runtime over in-memory pipes; there is no child
    /// process to supervise in that case.
    pub fn from_client(
        descriptor: HarnessDescriptor,
        client: Arc<AcpClient>,
        instance_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            descriptor,
            client,
            process: None,
            instance_id,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    async fn open_session(
        &self,
        spec: CreateSessionSpec,
        resume: Option<String>,
    ) -> Result<HarnessSessionHandle, HarnessError> {
        if resume.is_some() && !self.descriptor.capabilities.session_resume {
            return Err(HarnessError::unsupported("sessionResume"));
        }
        let method = if resume.is_some() {
            "session/load"
        } else {
            "session/new"
        };
        let params = match &resume {
            Some(session_id) => json!({"sessionId": session_id, "cwd": spec.workspace}),
            None => json!({"cwd": spec.workspace, "mcpServers": []}),
        };
        let result = self
            .client
            .request(method, params)
            .await
            .map_err(acp_error)?;
        // A load reply may legitimately omit the id: the caller already named
        // the session it asked the native runtime to restore.
        let native_session_id = result
            .get("sessionId")
            .or_else(|| result.get("session").and_then(|value| value.get("id")))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(resume)
            .ok_or_else(|| protocol("ACP session response missing sessionId"))?;
        let handle = HarnessSessionHandle {
            runtime_instance_id: self.instance_id.clone(),
            native_session_id: native_session_id.clone(),
        };
        let session = Arc::new(AcpHarnessSession::new(
            self.descriptor.id.clone(),
            self.descriptor.capabilities.clone(),
            handle.clone(),
            spec.thread_id,
            Arc::clone(&self.client),
        ));
        if let Some(model) = spec.model {
            session.set_model(model).await?;
        }
        if let Some(effort) = spec.reasoning_effort {
            session.set_reasoning_effort(effort).await?;
        }
        self.sessions
            .lock()
            .await
            .insert(native_session_id, session);
        Ok(handle)
    }
}

#[async_trait]
impl HarnessRuntime for AcpHarnessRuntime {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    async fn create_session(
        &self,
        spec: CreateSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError> {
        self.open_session(spec, None).await
    }
    async fn resume_session(
        &self,
        spec: ResumeSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError> {
        self.open_session(
            CreateSessionSpec {
                thread_id: spec.thread_id,
                workspace: spec.workspace,
                model: None,
                reasoning_effort: None,
            },
            Some(spec.native_session_id),
        )
        .await
    }
    async fn list_sessions(
        &self,
        _: SessionListQuery,
    ) -> Result<Vec<NativeSessionSummary>, HarnessError> {
        if !self.descriptor.capabilities.session_list {
            return Err(HarnessError::unsupported("sessionList"));
        }
        let value = self
            .client
            .request("session/list", json!({}))
            .await
            .map_err(acp_error)?;
        Ok(value
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|session| {
                session
                    .get("sessionId")
                    .or_else(|| session.get("id"))
                    .and_then(Value::as_str)
                    .map(|id| NativeSessionSummary {
                        native_session_id: id.into(),
                        title: session
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
            })
            .collect())
    }
    async fn close_session(&self, session: &HarnessSessionHandle) -> Result<(), HarnessError> {
        if !self.descriptor.capabilities.session_close {
            return Err(HarnessError::unsupported("sessionClose"));
        }
        self.client
            .request(
                "session/close",
                json!({"sessionId": session.native_session_id}),
            )
            .await
            .map_err(acp_error)?;
        self.sessions
            .lock()
            .await
            .remove(&session.native_session_id);
        Ok(())
    }
    async fn session(
        &self,
        handle: &HarnessSessionHandle,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
        if handle.runtime_instance_id != self.instance_id {
            return Err(protocol(
                "native session belongs to another runtime instance",
            ));
        }
        self.sessions
            .lock()
            .await
            .get(&handle.native_session_id)
            .cloned()
            .map(|session| session as Arc<dyn HarnessSession>)
            .ok_or_else(|| HarnessError::Categorized {
                category: HarnessErrorCategory::SessionNotFound,
                message: format!("native session not found: {}", handle.native_session_id),
            })
    }
    async fn shutdown(&self) -> Result<(), HarnessError> {
        match &self.process {
            Some(process) => {
                process
                    .lock()
                    .await
                    .graceful_shutdown(std::time::Duration::from_secs(3))
                    .await
            }
            None => Ok(()),
        }
    }
}

pub struct AcpHarnessSession {
    handle: HarnessSessionHandle,
    capabilities: HarnessCapabilities,
    harness_id: HarnessId,
    thread_id: String,
    client: Arc<AcpClient>,
    events: broadcast::Sender<HarnessEventEnvelope>,
    /// Native permission id -> the JSON-RPC id that must carry the reply.
    /// Entries are removed on the first resolution so a second one fails.
    pending_permissions: Arc<Mutex<HashMap<String, Value>>>,
}

impl AcpHarnessSession {
    fn new(
        harness_id: HarnessId,
        capabilities: HarnessCapabilities,
        handle: HarnessSessionHandle,
        thread_id: String,
        client: Arc<AcpClient>,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        let session = Self {
            handle,
            capabilities,
            harness_id,
            thread_id,
            client,
            events,
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        };
        session.bridge_notifications();
        session
    }

    fn bridge_notifications(&self) {
        let mut incoming = self.client.subscribe();
        let events = self.events.clone();
        let harness_id = self.harness_id.clone();
        let thread_id = self.thread_id.clone();
        let native_session_id = self.handle.native_session_id.clone();
        let pending_permissions = Arc::clone(&self.pending_permissions);
        let context = EventMapContext {
            native_session_id: native_session_id.clone(),
            extension_namespace: extension_namespace(&self.harness_id),
        };
        tokio::spawn(async move {
            let mapper = AcpEventMapper;
            let mut seq = 0u64;
            while let Ok(notification) = incoming.recv().await {
                let (method, params, request_id) = match notification {
                    AcpIncoming::Notification { method, params } => (method, params, None),
                    AcpIncoming::ServerRequest { id, method, params } => (method, params, Some(id)),
                    // Transport faults are connection-scoped and are surfaced
                    // by the runtime, not attributed to one session.
                    AcpIncoming::ProtocolError { .. } | AcpIncoming::Eof => continue,
                };
                if !AcpEventMapper::belongs_to_session(&params, &native_session_id) {
                    continue;
                }
                let Ok(mapped) = mapper.map_notification(&method, &params, &context) else {
                    continue;
                };
                for event in mapped {
                    if let (HarnessEvent::PermissionRequested(request), Some(id)) =
                        (&event, request_id.as_ref())
                    {
                        pending_permissions
                            .lock()
                            .await
                            .insert(request.native_request_id.clone(), id.clone());
                    }
                    seq = seq.saturating_add(1);
                    let _ = events.send(HarnessEventEnvelope {
                        seq,
                        harness_id: harness_id.clone(),
                        thread_id: thread_id.clone(),
                        native_session_id: native_session_id.clone(),
                        native_event_id: None,
                        occurred_at: chrono::Utc::now(),
                        event,
                    });
                }
            }
        });
    }
}

#[async_trait]
impl HarnessSession for AcpHarnessSession {
    fn handle(&self) -> &HarnessSessionHandle {
        &self.handle
    }
    fn capabilities(&self) -> &HarnessCapabilities {
        &self.capabilities
    }
    async fn prompt(&self, request: PromptRequest) -> Result<PromptHandle, HarnessError> {
        let result = self
            .client
            .request(
                "session/prompt",
                json!({
                    "sessionId": self.handle.native_session_id,
                    "prompt": [{"type": "text", "text": request.text}]
                }),
            )
            .await
            .map_err(acp_error)?;
        Ok(PromptHandle {
            native_turn_id: result
                .get("turnId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
    async fn cancel(&self, _: CancelRequest) -> Result<(), HarnessError> {
        self.client
            .notify(
                "session/cancel",
                json!({"sessionId": self.handle.native_session_id}),
            )
            .await
            .map_err(acp_error)
    }
    async fn resolve_permission(&self, response: PermissionResolution) -> Result<(), HarnessError> {
        if !self.capabilities.permissions {
            return Err(HarnessError::unsupported("permissions"));
        }
        // Fail closed: an unknown or already-resolved id must never be replayed
        // to the native harness as a second decision.
        let request_id = self
            .pending_permissions
            .lock()
            .await
            .remove(&response.native_request_id)
            .ok_or_else(|| HarnessError::Categorized {
                category: HarnessErrorCategory::Permission,
                message: format!(
                    "permission {} is unknown or already resolved",
                    response.native_request_id
                ),
            })?;
        let outcome = if response.granted {
            json!({"outcome": "selected", "optionId": "allow"})
        } else {
            json!({"outcome": "cancelled"})
        };
        self.client
            .respond(request_id, json!({"outcome": outcome}))
            .await
            .map_err(acp_error)
    }
    async fn set_model(&self, model: String) -> Result<(), HarnessError> {
        if !self.capabilities.model_selection {
            return Err(HarnessError::unsupported("modelSelection"));
        }
        self.client
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": self.handle.native_session_id,
                    "configId": "model",
                    "value": model
                }),
            )
            .await
            .map_err(acp_error)?;
        Ok(())
    }
    async fn set_reasoning_effort(&self, effort: String) -> Result<(), HarnessError> {
        if !self.capabilities.reasoning_effort {
            return Err(HarnessError::unsupported("reasoningEffort"));
        }
        self.client
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": self.handle.native_session_id,
                    "configId": "reasoning_effort",
                    "value": effort
                }),
            )
            .await
            .map_err(acp_error)?;
        Ok(())
    }
    async fn compact(&self) -> Result<(), HarnessError> {
        if !self.capabilities.compaction {
            return Err(HarnessError::unsupported("compaction"));
        }
        // Compaction is the native harness's own operation. Vellum asks for it
        // and never substitutes its own summarisation for the result.
        self.client
            .request(
                "session/compact",
                json!({"sessionId": self.handle.native_session_id}),
            )
            .await
            .map_err(acp_error)?;
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<HarnessEventEnvelope> {
        self.events.subscribe()
    }
}

fn unavailable(message: &str) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::RuntimeUnavailable,
        message: message.into(),
    }
}
fn protocol(message: &str) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::Protocol,
        message: message.into(),
    }
}
fn acp_error(error: vellum_acp_client::AcpError) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::Transport,
        message: error.to_string(),
    }
}
