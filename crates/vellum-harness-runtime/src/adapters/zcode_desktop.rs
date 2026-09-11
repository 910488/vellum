use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{broadcast, Mutex};
use vellum_harness_protocol::{
    HarnessCapabilities, HarnessDescriptor, HarnessErrorCategory, HarnessErrorEvent,
    HarnessErrorInfo, HarnessEvent, HarnessEventEnvelope, HarnessId, HarnessSessionHandle,
    HarnessTransportKind, MessageDeltaEvent, MessageEvent, NativeExtensionEvent, ProcessScope,
    ToolCallEvent, ToolResultEvent, TurnEvent, UsageEvent,
};
use vellum_zcode_desktop::{
    default_log_dir, ArtifactPin, ControlEvent, ZcodeDesktopError, ZcodeDesktopHost,
    CONTROL_PROTOCOL_VERSION,
};

use crate::{
    CancelRequest, CreateSessionSpec, HarnessDriver, HarnessError, HarnessProbeContext,
    HarnessProbeResult, HarnessRuntime, HarnessSession, HarnessStartSpec, NativeSessionSummary,
    PermissionResolution, PromptHandle, PromptRequest, ResumeSessionSpec, SessionListQuery,
};

pub struct ZcodeDesktopDriver {
    descriptor: HarnessDescriptor,
    log_dir: PathBuf,
    bindings_path: PathBuf,
    pin: Option<ArtifactPin>,
}

impl ZcodeDesktopDriver {
    pub fn new(log_dir: PathBuf, bindings_path: PathBuf, pin: Option<ArtifactPin>) -> Self {
        Self {
            log_dir,
            bindings_path,
            pin,
            descriptor: HarnessDescriptor {
                id: HarnessId(HarnessId::ZCODE_DESKTOP.into()),
                display_name: "ZCode Desktop".into(),
                vendor: "Zhipu".into(),
                transport: HarnessTransportKind::ZcodeDesktopTap,
                process_scope: ProcessScope::Shared,
                capabilities: HarnessCapabilities {
                    session_resume: true,
                    session_list: true,
                    usage: true,
                    tool_lifecycle: true,
                    ..Default::default()
                },
            },
        }
    }

    pub fn from_env() -> Self {
        let log_dir = std::env::var("ZCODE_TAP_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_log_dir());
        let bindings_path = log_dir.join("thread-bindings.sqlite");
        let pin = std::env::var("ZCODE_TAP_PIN_CJS_SHA256")
            .ok()
            .map(|cjs_sha256| ArtifactPin { cjs_sha256 });
        Self::new(log_dir, bindings_path, pin)
    }
}

#[async_trait]
impl HarnessDriver for ZcodeDesktopDriver {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    async fn probe(
        &self,
        _context: &HarnessProbeContext,
    ) -> Result<HarnessProbeResult, HarnessError> {
        match ZcodeDesktopHost::connect(&self.log_dir, &self.bindings_path, self.pin.as_ref()).await
        {
            Ok(host) => Ok(HarnessProbeResult {
                harness_id: self.descriptor.id.clone(),
                available: true,
                capabilities: Some(self.descriptor.capabilities.clone()),
                diagnostic: Some(format!(
                    "tap protocol {CONTROL_PROTOCOL_VERSION} artifact {}",
                    host.hello().artifact.cjs_sha256
                )),
            }),
            Err(error) => Ok(HarnessProbeResult {
                harness_id: self.descriptor.id.clone(),
                available: false,
                capabilities: None,
                diagnostic: Some(error.to_string()),
            }),
        }
    }

    async fn start(&self, spec: HarnessStartSpec) -> Result<Arc<dyn HarnessRuntime>, HarnessError> {
        let host = ZcodeDesktopHost::connect(&self.log_dir, &self.bindings_path, self.pin.as_ref())
            .await
            .map_err(map_error)?;
        Ok(Arc::new(ZcodeDesktopRuntime {
            descriptor: self.descriptor.clone(),
            host: Arc::new(host),
            workspace: spec.workspace,
            threads: Mutex::new(HashMap::new()),
        }))
    }
}

struct ZcodeDesktopRuntime {
    descriptor: HarnessDescriptor,
    host: Arc<ZcodeDesktopHost>,
    workspace: PathBuf,
    threads: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl HarnessRuntime for ZcodeDesktopRuntime {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    async fn create_session(
        &self,
        spec: CreateSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError> {
        let bound = self
            .host
            .bind_thread(&spec.thread_id, None, &self.workspace.to_string_lossy())
            .await
            .map_err(map_error)?;
        self.threads
            .lock()
            .await
            .insert(bound.session_id.clone(), spec.thread_id);
        Ok(HarnessSessionHandle {
            runtime_instance_id: self.host.listen().pid.to_string(),
            native_session_id: bound.session_id,
        })
    }

    async fn resume_session(
        &self,
        spec: ResumeSessionSpec,
    ) -> Result<HarnessSessionHandle, HarnessError> {
        let bound = self
            .host
            .bind_thread(
                &spec.thread_id,
                Some(&spec.native_session_id),
                &spec.workspace.to_string_lossy(),
            )
            .await
            .map_err(map_error)?;
        self.threads
            .lock()
            .await
            .insert(bound.session_id.clone(), spec.thread_id);
        Ok(HarnessSessionHandle {
            runtime_instance_id: self.host.listen().pid.to_string(),
            native_session_id: bound.session_id,
        })
    }

    async fn list_sessions(
        &self,
        _query: SessionListQuery,
    ) -> Result<Vec<NativeSessionSummary>, HarnessError> {
        let status = self.host.status().await.map_err(map_error)?;
        Ok(status
            .sessions
            .into_iter()
            .map(|native_session_id| NativeSessionSummary {
                native_session_id,
                title: None,
            })
            .collect())
    }

    async fn close_session(&self, session: &HarnessSessionHandle) -> Result<(), HarnessError> {
        self.threads.lock().await.remove(&session.native_session_id);
        Ok(())
    }

    async fn session(
        &self,
        session: &HarnessSessionHandle,
    ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
        let thread_id = self
            .threads
            .lock()
            .await
            .get(&session.native_session_id)
            .cloned()
            .unwrap_or_else(|| session.native_session_id.clone());
        Ok(Arc::new(ZcodeDesktopSession {
            handle: session.clone(),
            thread_id,
            host: Arc::clone(&self.host),
            capabilities: self.descriptor.capabilities.clone(),
            last_turn: Mutex::new(None),
        }))
    }

    async fn shutdown(&self) -> Result<(), HarnessError> {
        Ok(())
    }
}

struct ZcodeDesktopSession {
    handle: HarnessSessionHandle,
    thread_id: String,
    host: Arc<ZcodeDesktopHost>,
    capabilities: HarnessCapabilities,
    last_turn: Mutex<Option<String>>,
}

#[async_trait]
impl HarnessSession for ZcodeDesktopSession {
    fn handle(&self) -> &HarnessSessionHandle {
        &self.handle
    }

    fn capabilities(&self) -> &HarnessCapabilities {
        &self.capabilities
    }

    async fn prompt(&self, request: PromptRequest) -> Result<PromptHandle, HarnessError> {
        let turn_id = uuid::Uuid::new_v4().to_string();
        let started = self
            .host
            .start_turn_with_id(&self.thread_id, &turn_id, &request.text, Some(120_000))
            .await
            .map_err(map_error)?;
        *self.last_turn.lock().await = Some(started.vellum_turn_id.clone());
        Ok(PromptHandle {
            native_turn_id: Some(started.vellum_turn_id),
        })
    }

    async fn cancel(&self, request: CancelRequest) -> Result<(), HarnessError> {
        let turn_id = request
            .native_turn_id
            .or(self.last_turn.lock().await.clone())
            .ok_or_else(|| HarnessError::unsupported("cancel"))?;
        self.host
            .cancel_turn(&self.thread_id, &turn_id)
            .await
            .map_err(map_error)
    }

    async fn resolve_permission(
        &self,
        _response: PermissionResolution,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::unsupported("permissions"))
    }

    async fn set_model(&self, _model: String) -> Result<(), HarnessError> {
        Err(HarnessError::unsupported("modelSelection"))
    }

    async fn set_reasoning_effort(&self, _effort: String) -> Result<(), HarnessError> {
        Err(HarnessError::unsupported("reasoningEffort"))
    }

    async fn compact(&self) -> Result<(), HarnessError> {
        Err(HarnessError::unsupported("compaction"))
    }

    fn subscribe(&self) -> broadcast::Receiver<HarnessEventEnvelope> {
        let (sender, receiver) = broadcast::channel(32);
        let mut events = self.host.subscribe();
        let harness_id = HarnessId(HarnessId::ZCODE_DESKTOP.into());
        let thread_id = self.thread_id.clone();
        let native_session_id = self.handle.native_session_id.clone();
        tokio::spawn(async move {
            let mut seq = 0u64;
            while let Ok(event) = events.recv().await {
                if event_session_id(&event).is_some_and(|id| id != native_session_id) {
                    continue;
                }
                for mapped in map_event(event) {
                    seq += 1;
                    let _ = sender.send(HarnessEventEnvelope {
                        seq,
                        harness_id: harness_id.clone(),
                        thread_id: thread_id.clone(),
                        native_session_id: native_session_id.clone(),
                        native_event_id: None,
                        occurred_at: Utc::now(),
                        event: mapped,
                    });
                }
            }
        });
        receiver
    }
}

fn event_session_id(event: &ControlEvent) -> Option<&str> {
    match event {
        ControlEvent::TurnStarted(event) => Some(&event.session_id),
        ControlEvent::TurnDelta(event) => Some(&event.session_id),
        ControlEvent::AssistantCompleted(event) => Some(&event.session_id),
        ControlEvent::TurnCompleted(event) => Some(&event.session_id),
        ControlEvent::TurnUsage(event) => Some(&event.session_id),
        ControlEvent::ToolStarted(event) | ControlEvent::ToolUpdated(event) => {
            Some(&event.session_id)
        }
        ControlEvent::ToolCompleted(event) => Some(&event.session_id),
        ControlEvent::SessionAnnounced(event) => Some(&event.session_id),
        ControlEvent::Lifecycle(_) | ControlEvent::ProtocolViolation(_) => None,
    }
}

fn map_event(event: ControlEvent) -> Vec<HarnessEvent> {
    match event {
        ControlEvent::TurnStarted(started) => vec![HarnessEvent::TurnStarted(TurnEvent {
            turn_id: Some(started.vellum_turn_id),
        })],
        ControlEvent::TurnDelta(delta) => delta
            .text
            .filter(|text| !text.is_empty())
            .map(|text| {
                vec![HarnessEvent::AssistantDelta(MessageDeltaEvent {
                    message_id: delta.native_turn_id,
                    delta: text,
                })]
            })
            .unwrap_or_default(),
        ControlEvent::AssistantCompleted(message) => {
            vec![HarnessEvent::AssistantMessage(MessageEvent {
                message_id: message.message_id,
                text: message.text,
            })]
        }
        ControlEvent::TurnUsage(usage) => vec![HarnessEvent::UsageUpdated(UsageEvent {
            usage: usage.usage,
        })],
        ControlEvent::ToolStarted(tool) => vec![HarnessEvent::ToolCallStarted(ToolCallEvent {
            tool_call_id: tool.tool_call_id,
            name: tool.name,
            arguments: tool.arguments,
        })],
        ControlEvent::ToolUpdated(tool) => vec![HarnessEvent::ToolCallUpdated(ToolCallEvent {
            tool_call_id: tool.tool_call_id,
            name: tool.name,
            arguments: tool.arguments,
        })],
        ControlEvent::ToolCompleted(tool) => {
            vec![HarnessEvent::ToolCallCompleted(ToolResultEvent {
                tool_call_id: tool.tool_call_id,
                name: Some(tool.name),
                result: tool.result,
                is_error: tool.is_error,
            })]
        }
        ControlEvent::TurnCompleted(done) => {
            let turn = HarnessEvent::TurnCompleted(TurnEvent {
                turn_id: Some(done.vellum_turn_id.clone()),
            });
            if done.outcome == vellum_zcode_desktop::TurnOutcome::Completed {
                return vec![turn];
            }
            vec![
                HarnessEvent::Error(HarnessErrorEvent {
                    error: HarnessErrorInfo {
                        category: match done.outcome {
                            vellum_zcode_desktop::TurnOutcome::Cancelled
                            | vellum_zcode_desktop::TurnOutcome::Timeout => {
                                HarnessErrorCategory::RuntimeUnavailable
                            }
                            _ => HarnessErrorCategory::Provider,
                        },
                        message: done
                            .error_message
                            .unwrap_or_else(|| format!("ZCode turn ended {:?}", done.outcome)),
                        diagnostic: done.error_code,
                    },
                }),
                turn,
            ]
        }
        ControlEvent::SessionAnnounced(session) => {
            native_extension("sessionAnnounced", serde_json::to_value(session))
        }
        ControlEvent::Lifecycle(lifecycle) => {
            native_extension("lifecycle", serde_json::to_value(lifecycle))
        }
        ControlEvent::ProtocolViolation(detail) => native_extension(
            "protocolViolation",
            Ok(serde_json::json!({ "detail": detail })),
        ),
    }
}

fn native_extension(
    event_type: &str,
    payload: Result<serde_json::Value, serde_json::Error>,
) -> Vec<HarnessEvent> {
    vec![HarnessEvent::NativeExtension(NativeExtensionEvent {
        namespace: "zcode-desktop".into(),
        event_type: event_type.into(),
        payload: payload.unwrap_or_default(),
    })]
}

fn map_error(error: ZcodeDesktopError) -> HarnessError {
    let category = match error {
        ZcodeDesktopError::DesktopUnavailable
        | ZcodeDesktopError::TapUnavailable
        | ZcodeDesktopError::Closed => HarnessErrorCategory::RuntimeUnavailable,
        ZcodeDesktopError::ProtocolMismatch(_) | ZcodeDesktopError::ArtifactMismatch { .. } => {
            HarnessErrorCategory::Protocol
        }
        ZcodeDesktopError::SessionNotFound(_) => HarnessErrorCategory::SessionNotFound,
        ZcodeDesktopError::ThreadAlreadyBound => HarnessErrorCategory::SessionConflict,
        ZcodeDesktopError::CaptchaWaiting => HarnessErrorCategory::Authentication,
        _ => HarnessErrorCategory::Internal,
    };
    HarnessError::Categorized {
        category,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_zcode_desktop::{
        AssistantCompletedEvent, ToolCompletedEvent, TurnCompletedEvent, TurnDeltaEvent,
        TurnOutcome,
    };

    #[test]
    fn assistant_text_uses_the_neutral_chat_event() {
        let events = map_event(ControlEvent::TurnDelta(TurnDeltaEvent {
            vellum_turn_id: "turn-1".into(),
            session_id: "session-a".into(),
            native_turn_id: Some("native-turn".into()),
            text: Some("hello".into()),
        }));
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::AssistantDelta(MessageDeltaEvent { message_id, delta })]
                if message_id.as_deref() == Some("native-turn") && delta == "hello"
        ));
    }

    #[test]
    fn completed_assistant_row_becomes_a_completed_message_item() {
        let events = map_event(ControlEvent::AssistantCompleted(AssistantCompletedEvent {
            vellum_turn_id: "turn-1".into(),
            session_id: "session-a".into(),
            message_id: Some("message-1".into()),
            text: "hello".into(),
        }));
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::AssistantMessage(MessageEvent { message_id, text })]
                if message_id.as_deref() == Some("message-1") && text == "hello"
        ));
    }

    #[test]
    fn failed_turn_emits_error_and_terminal_event() {
        let events = map_event(ControlEvent::TurnCompleted(TurnCompletedEvent {
            vellum_turn_id: "turn-2".into(),
            session_id: "session-a".into(),
            outcome: TurnOutcome::Failed,
            provider_id: Some("builtin:zai-start-plan".into()),
            model_id: Some("GLM-5.3-Flash".into()),
            error_code: Some("UPSTREAM".into()),
            error_message: Some("provider failed".into()),
        }));
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::Error(_), HarnessEvent::TurnCompleted(_)]
        ));
    }

    #[test]
    fn control_events_retain_their_native_session_for_filtering() {
        let event = ControlEvent::TurnDelta(TurnDeltaEvent {
            vellum_turn_id: "turn-3".into(),
            session_id: "session-b".into(),
            native_turn_id: None,
            text: Some("not for session-a".into()),
        });
        assert_eq!(event_session_id(&event), Some("session-b"));
    }

    #[test]
    fn completed_tool_rows_use_the_neutral_tool_result() {
        let events = map_event(ControlEvent::ToolCompleted(ToolCompletedEvent {
            vellum_turn_id: "turn-4".into(),
            session_id: "session-a".into(),
            tool_call_id: "tool-1".into(),
            name: "Read".into(),
            result: serde_json::json!({"text": "ok"}),
            is_error: false,
        }));
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::ToolCallCompleted(ToolResultEvent { tool_call_id, name, is_error, .. })]
                if tool_call_id == "tool-1" && name.as_deref() == Some("Read") && !is_error
        ));
    }
}
