//! Minimal downstream WebSocket client for isolated live smoke.
//!
//! Architecture:
//! - the WebSocket reader is the only reducer (snapshot/event/ACK once)
//! - history is an append-only log with a watch version (no lost wakeups)
//! - waiters / recovery helpers are pure observers of history + harness state

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio_tungstenite::tungstenite::Message;
use ulid::Ulid;
use vellum_remote_protocol::{
    message_type, ClientCapabilities, ClientHello, CommandStatus, Envelope, ResumeHint,
    ServerWelcome, SubscriptionMode, ThreadAck, ThreadCommand, ThreadCommandRequest,
    ThreadCommandResult, ThreadEvent, ThreadRuntimeStatus, ThreadSnapshot, ThreadSubscribe,
    UserInput, PROTOCOL_VERSION,
};

use super::config::LiveSmokeError;
use crate::ClientHarness;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum LiveClientEvent {
    Welcome(ServerWelcome),
    Snapshot(ThreadSnapshot),
    Event(ThreadEvent),
    CommandResult(ThreadCommandResult),
    Raw(Envelope<serde_json::Value>),
    Closed,
}

#[derive(Debug, Clone)]
pub struct RecoveredThreadState {
    pub thread_id: String,
    pub snapshot: Option<ThreadSnapshot>,
    pub replay: Vec<ThreadEvent>,
    pub terminal: Option<ThreadRuntimeStatus>,
    pub final_seq: u64,
    pub from_snapshot: bool,
    pub from_replay: bool,
    pub from_future_event: bool,
}

/// Outbound writer protocol. Explicit Close must not depend on dropping every
/// remaining sender clone (reader keeps an ACK sender for the connection life).
enum OutboundMessage {
    Text(String),
    Close,
}

/// Append-only event log with monotonic version notifications.
struct LiveEventLog {
    events: Mutex<Vec<LiveClientEvent>>,
    version_tx: watch::Sender<u64>,
}

impl LiveEventLog {
    fn new() -> Arc<Self> {
        let (version_tx, _) = watch::channel(0u64);
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            version_tx,
        })
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.version_tx.subscribe()
    }

    fn current_version(&self) -> u64 {
        *self.version_tx.borrow()
    }

    async fn push(self: &Arc<Self>, event: LiveClientEvent) {
        let mut events = self.events.lock().await;
        events.push(event);
        let next = self.current_version().saturating_add(1);
        // send_replace keeps the latest version for late subscribers.
        self.version_tx.send_replace(next);
    }

    async fn len(&self) -> usize {
        self.events.lock().await.len()
    }

    async fn get(&self, index: usize) -> Option<LiveClientEvent> {
        self.events.lock().await.get(index).cloned()
    }

    /// Wait for the first event at or after `from` matching `predicate`.
    ///
    /// Uses watch versions so a push that happens between an empty scan and the
    /// wait cannot be lost (unlike Notify::notified without prior enable).
    async fn wait_from(
        &self,
        from: usize,
        timeout: Duration,
        mut predicate: impl FnMut(&LiveClientEvent) -> bool,
    ) -> Result<LiveClientEvent, LiveSmokeError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut idx = from;
        let mut version_rx = self.subscribe();
        loop {
            loop {
                let events = self.events.lock().await;
                if idx < events.len() {
                    let event = events[idx].clone();
                    idx += 1;
                    drop(events);
                    if predicate(&event) {
                        return Ok(event);
                    }
                    if matches!(event, LiveClientEvent::Closed) {
                        return Err(LiveSmokeError::Protocol("socket closed".into()));
                    }
                    continue;
                }
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(LiveSmokeError::Timeout("wait_from".into()));
            }

            // Capture version only after confirming the buffer is empty at idx.
            let seen = {
                let events = self.events.lock().await;
                if idx < events.len() {
                    continue;
                }
                self.current_version()
            };

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, version_rx.wait_for(|version| *version > seen))
                .await
            {
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => {
                    return Err(LiveSmokeError::Protocol(
                        "event log version channel closed".into(),
                    ))
                }
                Err(_) => return Err(LiveSmokeError::Timeout("wait_from".into())),
            }
        }
    }
}

pub struct LiveBrokerClient {
    device_id: String,
    outbound_tx: mpsc::UnboundedSender<OutboundMessage>,
    buffer: Arc<LiveEventLog>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ThreadCommandResult>>>>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
    pub harness: Arc<Mutex<ClientHarness>>,
    welcome: Mutex<Option<ServerWelcome>>,
}

impl LiveBrokerClient {
    pub async fn connect(url: &str, device_id: impl Into<String>) -> Result<Self, LiveSmokeError> {
        let device_id = device_id.into();
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| LiveSmokeError::Protocol(format!("ws connect failed: {error}")))?;
        let (mut sink, mut stream) = ws.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<OutboundMessage>();
        let buffer = LiveEventLog::new();
        let pending = Arc::new(Mutex::new(HashMap::<
            String,
            oneshot::Sender<ThreadCommandResult>,
        >::new()));
        let harness = Arc::new(Mutex::new(ClientHarness::default()));
        let pending_for_reader = pending.clone();
        let buffer_for_reader = buffer.clone();
        let harness_for_reader = harness.clone();
        // Reader keeps a sender clone for ACKs. Close must be an explicit message
        // (not "all senders dropped"), otherwise close() deadlocks waiting on reader.
        let ack_tx = outbound_tx.clone();

        let writer = tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                match msg {
                    OutboundMessage::Text(text) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    OutboundMessage::Close => {
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            // Channel closed without explicit Close: still drop the sink.
            drop(sink);
        });

        // Reader is the single consumer/reducer for wire frames.
        let reader = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => {
                        if let Ok(envelope) =
                            serde_json::from_str::<Envelope<serde_json::Value>>(&text)
                        {
                            match envelope.r#type.as_str() {
                                message_type::SERVER_WELCOME => {
                                    if let Ok(welcome) = serde_json::from_value::<ServerWelcome>(
                                        envelope.payload.clone(),
                                    ) {
                                        buffer_for_reader
                                            .push(LiveClientEvent::Welcome(welcome))
                                            .await;
                                    }
                                }
                                message_type::THREAD_SNAPSHOT => {
                                    if let Ok(snapshot) = serde_json::from_value::<ThreadSnapshot>(
                                        envelope.payload.clone(),
                                    ) {
                                        harness_for_reader
                                            .lock()
                                            .await
                                            .apply_snapshot(snapshot.clone());
                                        buffer_for_reader
                                            .push(LiveClientEvent::Snapshot(snapshot))
                                            .await;
                                    }
                                }
                                message_type::THREAD_EVENT => {
                                    if let Ok(event) = serde_json::from_value::<ThreadEvent>(
                                        envelope.payload.clone(),
                                    ) {
                                        let applied = harness_for_reader
                                            .lock()
                                            .await
                                            .apply_event(event.clone());
                                        if applied {
                                            // ACK exactly once from the reducer path.
                                            let _ = send_thread_ack_raw(
                                                &ack_tx,
                                                &event.thread_id,
                                                event.seq,
                                            );
                                        }
                                        buffer_for_reader.push(LiveClientEvent::Event(event)).await;
                                    }
                                }
                                message_type::THREAD_COMMAND_RESULT => {
                                    if let Ok(result) = serde_json::from_value::<ThreadCommandResult>(
                                        envelope.payload.clone(),
                                    ) {
                                        if let Some(tx) = pending_for_reader
                                            .lock()
                                            .await
                                            .remove(&result.idempotency_key)
                                        {
                                            let _ = tx.send(result.clone());
                                        }
                                        buffer_for_reader
                                            .push(LiveClientEvent::CommandResult(result))
                                            .await;
                                    }
                                }
                                _ => {
                                    buffer_for_reader.push(LiveClientEvent::Raw(envelope)).await;
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        buffer_for_reader.push(LiveClientEvent::Closed).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            device_id,
            outbound_tx,
            buffer,
            pending,
            reader,
            writer,
            harness,
            welcome: Mutex::new(None),
        })
    }

    pub async fn hello(&self, resume: Option<ResumeHint>) -> Result<ServerWelcome, LiveSmokeError> {
        let from = self.buffer.len().await;
        let payload = ClientHello {
            device_id: self.device_id.clone(),
            client_version: "vellum-live-smoke".into(),
            device_token: None,
            resume,
            capabilities: ClientCapabilities {
                approvals: true,
                artifacts: true,
                writer_lease: true,
            },
        };
        self.send_typed(message_type::CLIENT_HELLO, payload).await?;
        let event = self
            .buffer
            .wait_from(from, Duration::from_secs(15), |event| {
                matches!(event, LiveClientEvent::Welcome(_))
            })
            .await?;
        match event {
            LiveClientEvent::Welcome(welcome) => {
                *self.welcome.lock().await = Some(welcome.clone());
                Ok(welcome)
            }
            _ => Err(LiveSmokeError::Protocol("expected server.welcome".into())),
        }
    }

    /// Subscribe and collect snapshot + replay from history only.
    ///
    /// Reduction/ACK already happened in the reader when frames arrived.
    pub async fn subscribe_thread(
        &self,
        thread_id: &str,
        last_ack_seq: u64,
        mode: SubscriptionMode,
    ) -> Result<(Option<ThreadSnapshot>, Vec<ThreadEvent>), LiveSmokeError> {
        let from = self.buffer.len().await;
        let payload = ThreadSubscribe {
            thread_id: thread_id.to_string(),
            last_ack_seq,
            mode,
        };
        self.send_typed(message_type::THREAD_SUBSCRIBE, payload)
            .await?;

        let mut snapshot = None;
        let mut replay = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut idx = from;
        let mut version_rx = self.buffer.subscribe();
        while tokio::time::Instant::now() < deadline {
            while let Some(event) = self.buffer.get(idx).await {
                idx += 1;
                match event {
                    LiveClientEvent::Snapshot(s) => {
                        // Observer only: harness already applied this in the reader.
                        snapshot = Some(s);
                    }
                    LiveClientEvent::Event(thread_event) => {
                        if thread_event.thread_id == thread_id && thread_event.seq > last_ack_seq {
                            replay.push(thread_event);
                        }
                    }
                    LiveClientEvent::Closed => {
                        return Err(LiveSmokeError::Protocol("socket closed".into()));
                    }
                    _ => {}
                }
            }
            if snapshot.is_some() {
                // Drain a short quiet window of replay after snapshot.
                let seen = *version_rx.borrow();
                match tokio::time::timeout(
                    Duration::from_millis(400),
                    version_rx.wait_for(|v| *v > seen),
                )
                .await
                {
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => break,
                    Err(_) => break,
                }
            } else {
                let seen = {
                    if self.buffer.get(idx).await.is_some() {
                        continue;
                    }
                    self.buffer.current_version()
                };
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(
                    remaining.min(Duration::from_millis(500)),
                    version_rx.wait_for(|v| *v > seen),
                )
                .await
                {
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => {
                        return Err(LiveSmokeError::Protocol(
                            "event log version channel closed".into(),
                        ))
                    }
                    Err(_) => continue,
                }
            }
        }
        Ok((snapshot, replay))
    }

    /// Recover thread state after reconnect/subscribe.
    ///
    /// Success paths:
    /// 1. terminal snapshot
    /// 2. terminal event already in replay history
    /// 3. future terminal event observed on the shared log
    pub async fn recover_thread(
        &self,
        thread_id: &str,
        last_ack_seq: u64,
        mode: SubscriptionMode,
        wait_for_terminal: Duration,
    ) -> Result<RecoveredThreadState, LiveSmokeError> {
        let history_from = self.buffer.len().await;
        let (snapshot, replay) = self.subscribe_thread(thread_id, last_ack_seq, mode).await?;

        let mut terminal = snapshot.as_ref().and_then(|s| {
            if is_terminal_status(s.status) {
                Some(s.status)
            } else {
                None
            }
        });
        let from_snapshot = terminal.is_some();
        let mut from_replay = false;
        let mut from_future_event = false;

        if terminal.is_none() {
            for event in &replay {
                if let Some(status) = terminal_status_from_event(event) {
                    terminal = Some(status);
                    from_replay = true;
                    break;
                }
            }
        }

        if terminal.is_none() {
            // Observe only ? do not re-reduce history.
            let event = self
                .observe_event_from(history_from, wait_for_terminal, |event| {
                    event.thread_id == thread_id && terminal_status_from_event(event).is_some()
                })
                .await?;
            terminal = terminal_status_from_event(&event);
            from_future_event = true;
        }

        let final_seq = self.harness.lock().await.resume_seq;
        Ok(RecoveredThreadState {
            thread_id: thread_id.to_string(),
            snapshot,
            replay,
            terminal,
            final_seq,
            from_snapshot,
            from_replay,
            from_future_event,
        })
    }

    pub async fn command(
        &self,
        thread_id: Option<String>,
        command: ThreadCommand,
        timeout: Duration,
    ) -> Result<ThreadCommandResult, LiveSmokeError> {
        let idempotency_key = Ulid::new().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(idempotency_key.clone(), tx);
        let request = ThreadCommandRequest {
            idempotency_key: idempotency_key.clone(),
            thread_id,
            expected_writer_lease_id: None,
            command,
        };
        self.send_typed(message_type::THREAD_COMMAND, request)
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(LiveSmokeError::Protocol(
                "command reply channel closed".into(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&idempotency_key);
                Err(LiveSmokeError::Timeout(format!(
                    "command {idempotency_key}"
                )))
            }
        }
    }

    pub async fn thread_start(
        &self,
        cwd: &str,
        model: Option<String>,
    ) -> Result<ThreadCommandResult, LiveSmokeError> {
        self.command(
            None,
            ThreadCommand::ThreadStart {
                cwd: cwd.to_string(),
                model,
                approval_policy: Some("never".into()),
            },
            Duration::from_secs(60),
        )
        .await
    }

    pub async fn writer_acquire(
        &self,
        thread_id: &str,
    ) -> Result<ThreadCommandResult, LiveSmokeError> {
        self.command(
            Some(thread_id.to_string()),
            ThreadCommand::WriterAcquire,
            Duration::from_secs(30),
        )
        .await
    }

    pub async fn turn_start(
        &self,
        thread_id: &str,
        text: &str,
    ) -> Result<ThreadCommandResult, LiveSmokeError> {
        self.command(
            Some(thread_id.to_string()),
            ThreadCommand::TurnStart {
                input: vec![UserInput::text(text)],
                settings: Default::default(),
            },
            Duration::from_secs(120),
        )
        .await
    }

    /// Observe a thread event from the shared history. Does not reduce/ACK.
    pub async fn wait_for_event(
        &self,
        timeout: Duration,
        predicate: impl FnMut(&ThreadEvent) -> bool,
    ) -> Result<ThreadEvent, LiveSmokeError> {
        // Include already-buffered events so late observers cannot miss history.
        self.observe_event_from(0, timeout, predicate).await
    }

    async fn observe_event_from(
        &self,
        from: usize,
        timeout: Duration,
        mut predicate: impl FnMut(&ThreadEvent) -> bool,
    ) -> Result<ThreadEvent, LiveSmokeError> {
        let event = self
            .buffer
            .wait_from(from, timeout, |item| match item {
                LiveClientEvent::Event(event) => predicate(event),
                LiveClientEvent::Closed => true,
                _ => false,
            })
            .await?;
        match event {
            LiveClientEvent::Event(event) => Ok(event),
            LiveClientEvent::Closed => Err(LiveSmokeError::Protocol("socket closed".into())),
            _ => Err(LiveSmokeError::Protocol("unexpected event kind".into())),
        }
    }

    pub async fn wait_for_terminal(
        &self,
        timeout: Duration,
    ) -> Result<(ThreadRuntimeStatus, u64), LiveSmokeError> {
        // Snapshot already terminal? (reader applied it once)
        if let Some(snapshot) = self.harness.lock().await.snapshot.clone() {
            if is_terminal_status(snapshot.status) {
                let seq = self.harness.lock().await.resume_seq;
                return Ok((snapshot.status, seq));
            }
        }
        // History may already contain a terminal event.
        let from = 0usize;
        let event = self
            .observe_event_from(from, timeout, |event| {
                terminal_status_from_event(event).is_some()
            })
            .await?;
        let status = terminal_status_from_event(&event)
            .ok_or_else(|| LiveSmokeError::Protocol("missing terminal status".into()))?;
        let seq = self.harness.lock().await.resume_seq;
        Ok((status, seq))
    }

    /// Wait for a terminal event belonging to one new turn. Unlike
    /// `wait_for_terminal`, this never accepts a terminal snapshot or an older
    /// completed turn from the append-only buffer.
    pub async fn wait_for_terminal_after(
        &self,
        thread_id: &str,
        after_seq: u64,
        timeout: Duration,
    ) -> Result<(ThreadRuntimeStatus, u64), LiveSmokeError> {
        let event = self
            .observe_event_from(0, timeout, |event| {
                event.thread_id == thread_id
                    && event.seq > after_seq
                    && terminal_status_from_event(event).is_some()
            })
            .await?;
        let status = terminal_status_from_event(&event)
            .ok_or_else(|| LiveSmokeError::Protocol("missing terminal status".into()))?;
        Ok((status, event.seq))
    }

    pub async fn last_ack_seq(&self) -> u64 {
        self.harness.lock().await.resume_seq
    }

    pub async fn stats(&self) -> (u64, u64, bool) {
        let h = self.harness.lock().await;
        (h.duplicate_count, h.gap_count, h.gap)
    }

    pub async fn require_completed(
        result: ThreadCommandResult,
    ) -> Result<ThreadCommandResult, LiveSmokeError> {
        match result.status {
            CommandStatus::Completed | CommandStatus::Accepted | CommandStatus::Processing => {
                Ok(result)
            }
            other => {
                let message = result
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| format!("unexpected command status={other:?}"));
                Err(LiveSmokeError::Protocol(message))
            }
        }
    }

    async fn send_typed<T: Serialize>(
        &self,
        message_type: &str,
        payload: T,
    ) -> Result<(), LiveSmokeError> {
        let envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: Ulid::new().to_string(),
            r#type: message_type.to_string(),
            sent_at: Utc::now(),
            payload: serde_json::to_value(payload)?,
        };
        let text = serde_json::to_string(&envelope)?;
        self.outbound_tx
            .send(OutboundMessage::Text(text))
            .map_err(|_| LiveSmokeError::Protocol("outbound channel closed".into()))?;
        Ok(())
    }

    /// Graceful WebSocket close.
    ///
    /// Sends an explicit `OutboundMessage::Close` so the writer closes the sink
    /// even while the reader still holds an ACK sender clone. Times out and
    /// aborts IO tasks if the peer never reacts.
    pub async fn close_gracefully(self) {
        let Self {
            outbound_tx,
            reader,
            writer,
            ..
        } = self;
        let _ = outbound_tx.send(OutboundMessage::Close);
        drop(outbound_tx);

        // Keep abort handles in case the peer ignores the close handshake.
        let reader_abort = reader.abort_handle();
        let writer_abort = writer.abort_handle();
        let join_both = async {
            let _ = reader.await;
            let _ = writer.await;
        };
        if tokio::time::timeout(Duration::from_secs(3), join_both)
            .await
            .is_err()
        {
            reader_abort.abort();
            writer_abort.abort();
        }
    }

    /// Abrupt disconnect: abort IO tasks and drop socket halves immediately.
    ///
    /// Models desktop crash / network drop / process kill better than a clean
    /// WebSocket close handshake. Does not require the peer to close first.
    pub async fn disconnect_abrupt(self) {
        let Self {
            outbound_tx,
            reader,
            writer,
            ..
        } = self;
        reader.abort();
        writer.abort();
        let _ = reader.await;
        let _ = writer.await;
        drop(outbound_tx);
    }

    /// Backward-compatible alias used by Ready/Turn after fixture cleanup.
    pub async fn close(self) {
        self.close_gracefully().await;
    }
}

fn send_thread_ack_raw(
    outbound_tx: &mpsc::UnboundedSender<OutboundMessage>,
    thread_id: &str,
    through_seq: u64,
) -> Result<(), LiveSmokeError> {
    let payload = ThreadAck {
        thread_id: thread_id.to_string(),
        through_seq,
    };
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: Ulid::new().to_string(),
        r#type: message_type::THREAD_ACK.to_string(),
        sent_at: Utc::now(),
        payload: serde_json::to_value(payload)?,
    };
    let text = serde_json::to_string(&envelope)?;
    outbound_tx
        .send(OutboundMessage::Text(text))
        .map_err(|_| LiveSmokeError::Protocol("outbound channel closed".into()))?;
    Ok(())
}

fn is_terminal_status(status: ThreadRuntimeStatus) -> bool {
    matches!(
        status,
        ThreadRuntimeStatus::Completed
            | ThreadRuntimeStatus::Failed
            | ThreadRuntimeStatus::Interrupted
    )
}

/// Mirror broker `ThreadActor::project_event` terminal normalization.
///
/// Important: method `turn/completed` is not enough ? inspect `turn.status`.
pub(crate) fn terminal_status_from_event(event: &ThreadEvent) -> Option<ThreadRuntimeStatus> {
    let method = event.method.as_str();
    if method.contains("turn/completed") || method.ends_with("turn.completed") {
        let turn_status = event
            .data
            .pointer("/turn/status")
            .and_then(|v| v.as_str())
            .or_else(|| event.data.get("status").and_then(|v| v.as_str()))
            .unwrap_or("completed");
        return match turn_status {
            "failed" | "Failed" => Some(ThreadRuntimeStatus::Failed),
            "interrupted" | "Interrupted" => Some(ThreadRuntimeStatus::Interrupted),
            // Broker ThreadActor only treats "inProgress" as still running.
            "inProgress" => None,
            // Default matches broker ThreadActor::project_event for turn/completed.
            _ => Some(ThreadRuntimeStatus::Completed),
        };
    }
    if method.contains("turn/failed") || method.ends_with("turn.failed") {
        return Some(ThreadRuntimeStatus::Failed);
    }
    if method.contains("turn/interrupted") || method.ends_with("turn.interrupted") {
        return Some(ThreadRuntimeStatus::Interrupted);
    }
    if let Some(status) = event
        .data
        .pointer("/turn/status")
        .and_then(|v| v.as_str())
        .or_else(|| event.data.get("status").and_then(|v| v.as_str()))
    {
        return match status {
            "completed" | "Completed" => Some(ThreadRuntimeStatus::Completed),
            "failed" | "Failed" => Some(ThreadRuntimeStatus::Failed),
            "interrupted" | "Interrupted" => Some(ThreadRuntimeStatus::Interrupted),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_event(seq: u64, method: &str, data: serde_json::Value) -> ThreadEvent {
        ThreadEvent {
            seq,
            thread_id: "t1".into(),
            upstream_epoch: 1,
            method: method.into(),
            occurred_at: Utc::now(),
            turn_id: None,
            item_id: None,
            data,
        }
    }

    #[test]
    fn turn_completed_with_failed_status_is_not_completed() {
        let failed = sample_event(3, "turn/completed", json!({"turn": {"status": "failed"}}));
        assert_eq!(
            terminal_status_from_event(&failed),
            Some(ThreadRuntimeStatus::Failed)
        );

        let interrupted = sample_event(
            4,
            "turn/completed",
            json!({"turn": {"status": "interrupted"}}),
        );
        assert_eq!(
            terminal_status_from_event(&interrupted),
            Some(ThreadRuntimeStatus::Interrupted)
        );

        let completed = sample_event(
            5,
            "turn/completed",
            json!({"turn": {"status": "completed"}}),
        );
        assert_eq!(
            terminal_status_from_event(&completed),
            Some(ThreadRuntimeStatus::Completed)
        );

        let in_progress = sample_event(
            6,
            "turn/completed",
            json!({"turn": {"status": "inProgress"}}),
        );
        assert_eq!(terminal_status_from_event(&in_progress), None);
    }

    #[tokio::test]
    async fn buffered_event_before_wait_is_observed() {
        let log = LiveEventLog::new();
        log.push(LiveClientEvent::Event(sample_event(
            1,
            "item/updated",
            json!({}),
        )))
        .await;

        // Waiter starts after the event is already buffered.
        let observed = log
            .wait_from(
                0,
                Duration::from_secs(1),
                |item| matches!(item, LiveClientEvent::Event(e) if e.seq == 1),
            )
            .await
            .expect("should observe buffered event without timeout");
        match observed {
            LiveClientEvent::Event(event) => assert_eq!(event.seq, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_is_reduced_exactly_once_across_multiple_waiters() {
        // Simulate single-consumer reduction: reader applies once, waiters only observe.
        let mut harness = ClientHarness::default();
        let event = sample_event(1, "item/updated", json!({}));
        assert!(harness.apply_event(event.clone()));
        assert_eq!(harness.duplicate_count, 0);

        let log = LiveEventLog::new();
        log.push(LiveClientEvent::Event(event.clone())).await;

        let seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let log = log.clone();
            let seen = seen.clone();
            handles.push(tokio::spawn(async move {
                let observed = log
                    .wait_from(
                        0,
                        Duration::from_secs(1),
                        |item| matches!(item, LiveClientEvent::Event(e) if e.seq == 1),
                    )
                    .await
                    .expect("waiter observes event");
                if matches!(observed, LiveClientEvent::Event(_)) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(seen.load(Ordering::SeqCst), 3);

        // Waiters must not re-apply; a second apply would count as harness-side duplicate.
        assert!(!harness.apply_event(event));
        assert_eq!(harness.duplicate_count, 1);
        // In production path the second apply never happens because only the reader reduces.
        // This test documents the contract: multi-waiter observation != multi-reduce.
    }

    #[tokio::test]
    async fn push_during_empty_scan_is_not_lost() {
        let log = LiveEventLog::new();
        let log2 = log.clone();
        let waiter = tokio::spawn(async move {
            log2.wait_from(
                0,
                Duration::from_secs(2),
                |item| matches!(item, LiveClientEvent::Event(e) if e.seq == 7),
            )
            .await
        });
        // Give waiter a chance to reach the empty-scan/wait path.
        tokio::time::sleep(Duration::from_millis(20)).await;
        log.push(LiveClientEvent::Event(sample_event(
            7,
            "item/updated",
            json!({}),
        )))
        .await;
        let observed = waiter.await.unwrap().expect("no lost wakeup");
        match observed {
            LiveClientEvent::Event(event) => assert_eq!(event.seq, 7),
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn spawn_passive_ws_server() -> (String, tokio::task::JoinHandle<()>) {
        use futures_util::StreamExt as _;
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind passive ws");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("ws://{addr}");
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = accept_async(stream).await.expect("ws accept");
            // Stay connected until the client drops/closes the socket.
            while let Some(frame) = ws.next().await {
                if frame.is_err() {
                    break;
                }
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn client_disconnect_completes_without_peer_close() {
        let (url, server) = spawn_passive_ws_server().await;
        let client = LiveBrokerClient::connect(&url, "disconnect-abrupt")
            .await
            .expect("connect");
        let started = tokio::time::Instant::now();
        client.disconnect_abrupt().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "disconnect_abrupt hung: {:?}",
            started.elapsed()
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server should observe peer drop");
    }

    #[tokio::test]
    async fn client_graceful_close_does_not_depend_on_channel_sender_drop() {
        let (url, server) = spawn_passive_ws_server().await;
        let client = LiveBrokerClient::connect(&url, "disconnect-graceful")
            .await
            .expect("connect");
        let started = tokio::time::Instant::now();
        // Must complete even though reader holds an ACK sender clone.
        client.close_gracefully().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "close_gracefully hung: {:?}",
            started.elapsed()
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server should observe graceful close");
    }
}
