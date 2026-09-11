//! Codex App Server facade.
//!
//! To the Codex UI this looks like a Codex App Server. Behind it, every turn is
//! owned by the harness the thread is bound to. The supported method set is
//! closed and every name is checked against the pinned schema: a UI affordance
//! the selected harness does not have returns `UnsupportedCapability`, and a
//! method Codex does not actually define cannot be named at all.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use vellum_harness_protocol::{CompactionAuthority, HarnessEvent, HarnessSelection};
use vellum_harness_runtime::{
    CancelRequest, CreateSessionSpec, HarnessError, HarnessSession, PermissionResolution,
    PromptRequest,
};

use crate::broker::HarnessBroker;
use crate::journal::{EventJournal, JournalEntry};
use crate::methods::{self, notify, MethodClass};
use crate::permission::{PermissionError, PermissionRegistry};
use crate::ui_mapper::{
    CodexServerMessage, CodexServerNotification, CodexUiEventMapper, DefaultCodexUiEventMapper,
    UiMapContext,
};

#[derive(Debug, thiserror::Error)]
pub enum CodexFacadeError {
    #[error("unsupported Codex App Server method: {0}")]
    UnsupportedMethod(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error(transparent)]
    Harness(#[from] HarnessError),
}

pub struct CodexAppServerFacade {
    broker: Arc<dyn HarnessBroker>,
    mapper: Arc<dyn CodexUiEventMapper>,
    permissions: Arc<PermissionRegistry>,
    journal: Arc<EventJournal>,
    outbound: broadcast::Sender<CodexServerMessage>,
    /// Turn currently in flight per thread. Codex notifications carry a turn
    /// id that the neutral envelope does not.
    turns: Arc<Mutex<HashMap<String, String>>>,
    /// Sessions already being pumped, keyed by thread and native session, so a
    /// reconnect reuses the running pump instead of doubling the event stream.
    attached: Mutex<HashSet<String>>,
}

impl CodexAppServerFacade {
    pub fn new(broker: Arc<dyn HarnessBroker>) -> Self {
        Self::with_mapper(broker, Arc::new(DefaultCodexUiEventMapper))
    }

    pub fn with_mapper(
        broker: Arc<dyn HarnessBroker>,
        mapper: Arc<dyn CodexUiEventMapper>,
    ) -> Self {
        let (outbound, _) = broadcast::channel(1024);
        Self {
            broker,
            mapper,
            permissions: Arc::new(PermissionRegistry::new()),
            journal: Arc::new(EventJournal::new()),
            outbound,
            turns: Arc::new(Mutex::new(HashMap::new())),
            attached: Mutex::new(HashSet::new()),
        }
    }

    /// Everything the facade sends towards the UI: notifications and the
    /// server requests the UI must answer.
    pub fn subscribe(&self) -> broadcast::Receiver<CodexServerMessage> {
        self.outbound.subscribe()
    }

    pub fn permissions(&self) -> &Arc<PermissionRegistry> {
        &self.permissions
    }

    /// The complete neutral event record for a thread, for audit, evaluation
    /// and debugging (§24). It includes events that have no Codex
    /// representation and therefore never reached the UI — which is the point:
    /// the UI may show nothing, but nothing is lost.
    ///
    /// This is `NOT_RUNTIME_REPLAY_SOURCE`. Never feed it back into a harness.
    pub async fn audit_journal(&self, thread_id: &str) -> Vec<JournalEntry> {
        self.journal.read(thread_id).await
    }

    pub async fn handle_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, CodexFacadeError> {
        if methods::classify(method) == MethodClass::Unsupported {
            return Err(CodexFacadeError::UnsupportedMethod(method.into()));
        }
        match method {
            methods::INITIALIZE => self.handle_initialize(params).await,
            methods::MODEL_LIST => self.handle_model_list(params).await,
            methods::THREAD_START => self.handle_thread_start(params).await,
            methods::THREAD_RESUME => self.handle_thread_resume(params).await,
            methods::THREAD_READ => self.handle_thread_read(params).await,
            methods::THREAD_LIST => self.handle_thread_list(params).await,
            methods::THREAD_COMPACT_START => self.handle_thread_compact(params).await,
            methods::CONFIG_VALUE_WRITE => self.handle_config_value_write(params).await,
            methods::TURN_START => self.handle_turn_start(params).await,
            methods::TURN_INTERRUPT => self.handle_turn_interrupt(params).await,
            // Real Codex methods with no native-harness equivalent to serve.
            // They are refused by capability, not silently answered.
            methods::THREAD_FORK => self.require_capability("fork", &params).await,
            methods::COMMAND_EXEC => self.require_capability("terminals", &params).await,
            methods::THREAD_NAME_SET | methods::THREAD_METADATA_UPDATE => Ok(json!({})),
            other => Err(CodexFacadeError::UnsupportedMethod(other.into())),
        }
    }

    /// The UI answering one of the facade's server requests. Today that is a
    /// permission decision; the response shape is Codex's, not Vellum's.
    pub async fn handle_response(
        &self,
        ui_request_id: &str,
        result: Value,
    ) -> Result<(), CodexFacadeError> {
        // Consumes the binding first: a replayed approval must fail before it
        // can reach the native harness a second time.
        let binding = self.permissions.resolve(ui_request_id).await?;
        // A Codex approval response carries the granted permission profile.
        // Absence of a grant is a refusal, never an implicit yes.
        let granted = result
            .get("permissions")
            .is_some_and(|permissions| !permissions.is_null());
        self.broker
            .session_for_thread(&binding.thread_id)
            .await?
            .resolve_permission(PermissionResolution {
                native_request_id: binding.native_request_id,
                granted,
                payload: result,
            })
            .await?;
        Ok(())
    }

    async fn handle_initialize(&self, _params: Value) -> Result<Value, CodexFacadeError> {
        Ok(json!({
            "userAgent": {"name": "vellum-harness-facade", "version": env!("CARGO_PKG_VERSION")}
        }))
    }

    /// Models belong to the selected harness, so an unbound client gets an
    /// empty catalogue rather than Vellum's own provider list.
    async fn handle_model_list(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let Some(thread_id) = optional_string(&params, "threadId") else {
            return Ok(json!({"data": []}));
        };
        let session = self.broker.session_for_thread(&thread_id).await?;
        Ok(json!({
            "data": [],
            "modelSelectionSupported": session.capabilities().model_selection,
            "reasoningEffortSupported": session.capabilities().reasoning_effort
        }))
    }

    async fn handle_thread_start(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let cwd = string(&params, "cwd")?;
        let selection: HarnessSelection = params
            .get("harnessSelection")
            .cloned()
            .ok_or_else(|| {
                CodexFacadeError::InvalidRequest("thread/start requires harnessSelection".into())
            })
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|error| CodexFacadeError::InvalidRequest(error.to_string()))
            })?;
        let thread_id = format!("vellum_{}", uuid::Uuid::new_v4());
        let session = self
            .broker
            .create_session(
                selection.clone(),
                CreateSessionSpec {
                    thread_id: thread_id.clone(),
                    workspace: cwd.clone().into(),
                    model: selection.model_id.clone(),
                    reasoning_effort: selection.reasoning_effort.clone(),
                },
            )
            .await?;
        self.attach(&thread_id, &session).await;
        let _ = self
            .outbound
            .send(CodexServerMessage::Notification(CodexServerNotification {
                method: notify::THREAD_STARTED.into(),
                params: json!({"thread": {"id": thread_id, "cwd": cwd}}),
            }));
        Ok(json!({
            "thread": {"id": thread_id, "cwd": cwd},
            "harnessId": selection.harness_id,
            "nativeSessionId": session.handle().native_session_id,
            "capabilities": session.capabilities()
        }))
    }

    async fn handle_thread_resume(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let session = self.broker.resume_session(&thread_id).await?;
        self.attach(&thread_id, &session).await;
        Ok(json!({
            "thread": {"id": thread_id},
            "nativeSessionId": session.handle().native_session_id,
            "capabilities": session.capabilities()
        }))
    }

    /// Served from the journal, which is a reconnect mirror. It is explicitly
    /// not the transcript the native harness reasons over, and the response
    /// says so.
    async fn handle_thread_read(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let mut events = Vec::new();
        for entry in self.journal.read(&thread_id).await {
            let context = UiMapContext {
                turn_id: entry.turn_id.clone(),
                // A replayed permission is history, not a live decision: it is
                // never re-issued as an answerable request.
                ui_request_id: None,
            };
            let Ok(mapped) = self.mapper.map_event(&entry.envelope, &context) else {
                continue;
            };
            events.extend(mapped.into_iter().filter_map(|message| {
                message.as_notification().map(|notification| {
                    json!({"method": notification.method, "params": notification.params})
                })
            }));
        }
        Ok(json!({
            "thread": {"id": thread_id},
            "events": events,
            "source": "vellumJournalMirror"
        }))
    }

    async fn handle_thread_list(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let sessions = self.broker.list_native_sessions(&thread_id).await?;
        Ok(json!({
            "threads": sessions
                .into_iter()
                .map(|summary| json!({"id": summary.native_session_id, "name": summary.title}))
                .collect::<Vec<_>>()
        }))
    }

    /// Model and reasoning effort are thread configuration in Codex V2; there
    /// is no `thread/settings/update` request.
    async fn handle_config_value_write(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let session = self.broker.session_for_thread(&thread_id).await?;
        match string(&params, "key")?.as_str() {
            "model" => session.set_model(string(&params, "value")?).await?,
            "reasoning_effort" | "model_reasoning_effort" => {
                session
                    .set_reasoning_effort(string(&params, "value")?)
                    .await?
            }
            other => return Err(CodexFacadeError::Harness(HarnessError::unsupported(other))),
        }
        Ok(json!({}))
    }

    async fn handle_turn_start(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let input = params
            .get("input")
            .and_then(Value::as_array)
            .ok_or_else(|| CodexFacadeError::InvalidRequest("turn/start requires input".into()))?;
        let text = input
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let session = self.broker.session_for_thread(&thread_id).await?;
        if let Some(model) = optional_string(&params, "model") {
            session.set_model(model).await?;
        }
        if let Some(effort) = optional_string(&params, "effort") {
            session.set_reasoning_effort(effort).await?;
        }
        // The turn id is registered before prompting, so events that arrive
        // while `prompt` is still in flight are attributed to the right turn.
        let turn_id = format!("turn_{}", uuid::Uuid::new_v4());
        self.turns
            .lock()
            .await
            .insert(thread_id.clone(), turn_id.clone());
        let prompt = session
            .prompt(PromptRequest {
                text,
                metadata: params,
            })
            .await?;
        let response_turn_id = prompt.native_turn_id.unwrap_or(turn_id);
        self.turns
            .lock()
            .await
            .insert(thread_id, response_turn_id.clone());
        Ok(json!({"turn": {"id": response_turn_id}}))
    }

    async fn handle_turn_interrupt(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        self.broker
            .session_for_thread(&thread_id)
            .await?
            .cancel(CancelRequest {
                native_turn_id: optional_string(&params, "turnId"),
            })
            .await?;
        Ok(json!({}))
    }

    async fn handle_thread_compact(&self, params: Value) -> Result<Value, CodexFacadeError> {
        let thread_id = string(&params, "threadId")?;
        let session = self.broker.session_for_thread(&thread_id).await?;
        // Compaction never crosses authority. There is no fallback engine.
        match self.broker.compaction_authority(&thread_id).await? {
            CompactionAuthority::NativeHarness | CompactionAuthority::CodexNative => {
                session.compact().await?;
                Ok(json!({}))
            }
            CompactionAuthority::VellumGeneric => {
                session.compact().await?;
                Ok(json!({"authority": "vellumGeneric"}))
            }
            CompactionAuthority::Unsupported => Err(CodexFacadeError::Harness(
                HarnessError::unsupported("compaction"),
            )),
        }
    }

    async fn require_capability(
        &self,
        capability: &str,
        params: &Value,
    ) -> Result<Value, CodexFacadeError> {
        let thread_id = string(params, "threadId")?;
        let session = self.broker.session_for_thread(&thread_id).await?;
        let capabilities = session.capabilities();
        let supported = match capability {
            "plans" => capabilities.plans,
            "commands" => capabilities.commands,
            "terminals" => capabilities.terminals,
            // No harness declares a fork capability: the neutral protocol has
            // no fork, and replaying a transcript to fake one is exactly what
            // the authority rule forbids.
            _ => false,
        };
        if !supported {
            return Err(CodexFacadeError::Harness(HarnessError::unsupported(
                capability,
            )));
        }
        Err(CodexFacadeError::UnsupportedMethod(format!(
            "{capability} bridging is not implemented yet"
        )))
    }

    /// Pumps one bound session's events to the UI, registering permission
    /// bindings and journalling the neutral event on the way through.
    ///
    /// Attaching is idempotent per native session.
    async fn attach(&self, thread_id: &str, session: &Arc<dyn HarnessSession>) {
        let handle = session.handle();
        let key = format!(
            "{thread_id}|{}|{}",
            handle.runtime_instance_id, handle.native_session_id
        );
        if !self.attached.lock().await.insert(key) {
            return;
        }
        let mut events = session.subscribe();
        let mapper = Arc::clone(&self.mapper);
        let permissions = Arc::clone(&self.permissions);
        let journal = Arc::clone(&self.journal);
        let turns = Arc::clone(&self.turns);
        let outbound = self.outbound.clone();
        let thread_id = thread_id.to_owned();
        tokio::spawn(async move {
            while let Ok(envelope) = events.recv().await {
                let turn_id = turns.lock().await.get(&thread_id).cloned();
                let ui_request_id = match &envelope.event {
                    HarnessEvent::PermissionRequested(permission) => Some(
                        permissions
                            .register(
                                &thread_id,
                                envelope.harness_id.clone(),
                                &permission.native_request_id,
                            )
                            .await,
                    ),
                    _ => None,
                };
                // Journalled before mapping, so events with no Codex
                // representation still reach audit and evaluation.
                journal
                    .append(
                        &thread_id,
                        JournalEntry {
                            envelope: envelope.clone(),
                            turn_id: turn_id.clone(),
                        },
                    )
                    .await;
                let context = UiMapContext {
                    turn_id,
                    ui_request_id,
                };
                let Ok(mapped) = mapper.map_event(&envelope, &context) else {
                    continue;
                };
                for message in mapped {
                    let _ = outbound.send(message);
                }
            }
        });
    }
}

fn string(value: &Value, key: &str) -> Result<String, CodexFacadeError> {
    optional_string(value, key)
        .ok_or_else(|| CodexFacadeError::InvalidRequest(format!("missing {key}")))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
