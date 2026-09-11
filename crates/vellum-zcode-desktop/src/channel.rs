use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::protocol::{
    AssistantCompletedEvent, BindParams, BindResult, ControlFrame, HelloParams, HelloResult,
    LifecycleEvent, SessionAnnouncedEvent, StatusResult, ToolCompletedEvent, ToolLifecycleEvent,
    TurnCancelParams, TurnCompletedEvent, TurnDeltaEvent, TurnStartParams, TurnStartResult,
    TurnStartedEvent, TurnUsageEvent, CONTROL_PROTOCOL_VERSION, EVENT_ASSISTANT_COMPLETED,
    EVENT_LIFECYCLE, EVENT_SESSION_ANNOUNCED, EVENT_TOOL_COMPLETED, EVENT_TOOL_STARTED,
    EVENT_TOOL_UPDATED, EVENT_TURN_COMPLETED, EVENT_TURN_DELTA, EVENT_TURN_STARTED,
    EVENT_TURN_USAGE, METHOD_BIND, METHOD_HELLO, METHOD_STATUS, METHOD_TURN_CANCEL,
    METHOD_TURN_START,
};
use crate::{ArtifactPin, ZcodeDesktopError};

#[derive(Debug, Clone)]
pub enum ControlEvent {
    TurnStarted(TurnStartedEvent),
    TurnDelta(TurnDeltaEvent),
    AssistantCompleted(AssistantCompletedEvent),
    TurnCompleted(TurnCompletedEvent),
    TurnUsage(TurnUsageEvent),
    ToolStarted(ToolLifecycleEvent),
    ToolUpdated(ToolLifecycleEvent),
    ToolCompleted(ToolCompletedEvent),
    SessionAnnounced(SessionAnnouncedEvent),
    Lifecycle(LifecycleEvent),
    ProtocolViolation(String),
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, ZcodeDesktopError>>>>>;

pub struct ControlClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Pending,
    events: broadcast::Sender<ControlEvent>,
    next_id: AtomicU64,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl ControlClient {
    pub fn from_rw<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(64);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(read_loop(
            reader,
            Arc::clone(&pending),
            events.clone(),
            Arc::clone(&closed),
        ));
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending,
            events,
            next_id: AtomicU64::new(1),
            closed,
        }
    }

    pub fn from_duplex(stream: DuplexStream) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self::from_rw(reader, writer)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ControlEvent> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub async fn hello(&self, pin: Option<&ArtifactPin>) -> Result<HelloResult, ZcodeDesktopError> {
        let params = HelloParams {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            client: "vellum".into(),
            expected_cjs_sha256: pin.map(|pin| pin.cjs_sha256.clone()),
        };
        let result: HelloResult = self.request(METHOD_HELLO, params).await?;
        if result.protocol_version != CONTROL_PROTOCOL_VERSION {
            return Err(ZcodeDesktopError::ProtocolMismatch(format!(
                "tap speaks {} vellum speaks {CONTROL_PROTOCOL_VERSION}",
                result.protocol_version
            )));
        }
        if let Some(pin) = pin {
            if !pin
                .cjs_sha256
                .eq_ignore_ascii_case(&result.artifact.cjs_sha256)
            {
                return Err(ZcodeDesktopError::ArtifactMismatch {
                    expected: pin.cjs_sha256.clone(),
                    actual: result.artifact.cjs_sha256,
                });
            }
        }
        Ok(result)
    }

    pub async fn bind(&self, params: BindParams) -> Result<BindResult, ZcodeDesktopError> {
        self.request(METHOD_BIND, params).await
    }

    pub async fn start_turn(
        &self,
        params: TurnStartParams,
    ) -> Result<TurnStartResult, ZcodeDesktopError> {
        self.request(METHOD_TURN_START, params).await
    }

    pub async fn cancel_turn(&self, params: TurnCancelParams) -> Result<(), ZcodeDesktopError> {
        let _: Value = self.request(METHOD_TURN_CANCEL, params).await?;
        Ok(())
    }

    pub async fn status(&self) -> Result<StatusResult, ZcodeDesktopError> {
        self.request(METHOD_STATUS, json!({})).await
    }

    async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ZcodeDesktopError> {
        if self.is_closed() {
            return Err(ZcodeDesktopError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst).to_string();
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), sender);
        let frame = ControlFrame::request(id.clone(), method, serde_json::to_value(params)?);
        if let Err(error) = self.write(&frame).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let value = receiver.await.map_err(|_| ZcodeDesktopError::Closed)??;
        Ok(serde_json::from_value(value)?)
    }

    async fn write(&self, frame: &ControlFrame) -> Result<(), ZcodeDesktopError> {
        let mut writer = self.writer.lock().await;
        writer.write_all(&serde_json::to_vec(frame)?).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

async fn read_loop<R>(
    reader: R,
    pending: Pending,
    events: broadcast::Sender<ControlEvent>,
    closed: Arc<std::sync::atomic::AtomicBool>,
) where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let frame: ControlFrame = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(error) => {
                let _ = events.send(ControlEvent::ProtocolViolation(error.to_string()));
                continue;
            }
        };
        if frame.is_event() {
            if let Some(event) = decode_event(&frame) {
                let _ = events.send(event);
            }
            continue;
        }
        if let Some(id) = frame.id.as_ref().map(id_key) {
            if let Some(sender) = pending.lock().await.remove(&id) {
                let outcome = if let Some(error) = frame.error {
                    Err(ZcodeDesktopError::from_body(error))
                } else {
                    Ok(frame.result.unwrap_or(Value::Null))
                };
                let _ = sender.send(outcome);
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
}

fn id_key(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn decode_event(frame: &ControlFrame) -> Option<ControlEvent> {
    let method = frame.method.as_deref()?;
    let params = frame.params.clone().unwrap_or(Value::Null);
    match method {
        EVENT_TURN_STARTED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::TurnStarted),
        EVENT_TURN_DELTA => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::TurnDelta),
        EVENT_ASSISTANT_COMPLETED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::AssistantCompleted),
        EVENT_TURN_COMPLETED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::TurnCompleted),
        EVENT_TURN_USAGE => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::TurnUsage),
        EVENT_TOOL_STARTED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::ToolStarted),
        EVENT_TOOL_UPDATED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::ToolUpdated),
        EVENT_TOOL_COMPLETED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::ToolCompleted),
        EVENT_SESSION_ANNOUNCED => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::SessionAnnounced),
        EVENT_LIFECYCLE => serde_json::from_value(params)
            .ok()
            .map(ControlEvent::Lifecycle),
        _ => None,
    }
}
