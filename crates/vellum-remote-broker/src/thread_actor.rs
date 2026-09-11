//! Per-thread actor coordinating detach, resume, and upstream fan-in.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use ulid::Ulid;
use vellum_remote_protocol::{
    message_type, ApprovalDecision, CommandStatus, Envelope, RemoteError, RemoteErrorCode,
    SubscriptionMode, ThreadCommand, ThreadCommandRequest, ThreadCommandResult, ThreadEvent,
    ThreadRuntimeStatus, ThreadSnapshot,
};

use crate::app_server::jsonrpc::JsonRpcId;
use crate::app_server::supervisor::{extract_thread_id_from_result, extract_turn_id};
use crate::app_server::transport::AppServerTransport;
use crate::approval_registry::ApprovalRegistry;
use crate::command_router::{
    accepted, command_kind, completed, failed, indeterminate, lease_required, processing,
    requires_writer,
};
use crate::event_store::{EventStore, ThreadProjectionUpdate};
use crate::lease_manager::LeaseManager;
use crate::metrics::BrokerMetrics;
use crate::snapshot_store::SnapshotStore;

#[derive(Debug)]
pub enum ThreadActorMessage {
    AttachClient {
        device_id: String,
        mode: SubscriptionMode,
        last_ack_seq: u64,
        reply: oneshot::Sender<Result<AttachResult, RemoteError>>,
    },
    DetachClient {
        device_id: String,
    },
    ClientCommand {
        device_id: String,
        request: ThreadCommandRequest,
        reply: oneshot::Sender<ThreadCommandResult>,
    },
    Ack {
        device_id: String,
        through_seq: u64,
    },
    UpstreamEvent {
        epoch: u64,
        method: String,
        payload: Value,
    },
    UpstreamServerRequest {
        epoch: u64,
        request_id: JsonRpcId,
        method: String,
        payload: Value,
    },
    UpstreamDisconnected {
        epoch: u64,
        reason: String,
    },
    RecoverOnStartup,
    RecoverAfterReconnect {
        epoch: u64,
    },
    CreateSnapshot,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct AttachResult {
    pub snapshot: ThreadSnapshot,
    pub replay: Vec<ThreadEvent>,
}

#[derive(Clone)]
pub struct ThreadActorHandle {
    pub thread_id: String,
    tx: mpsc::UnboundedSender<ThreadActorMessage>,
    events: broadcast::Sender<Envelope<Value>>,
}

impl ThreadActorHandle {
    pub fn sender(&self) -> mpsc::UnboundedSender<ThreadActorMessage> {
        self.tx.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Envelope<Value>> {
        self.events.subscribe()
    }
}

pub struct ThreadActor {
    thread_id: String,
    store: EventStore,
    snapshots: SnapshotStore,
    approvals: ApprovalRegistry,
    leases: LeaseManager,
    upstream: Arc<dyn AppServerTransport>,
    metrics: Arc<BrokerMetrics>,
    status: ThreadRuntimeStatus,
    loaded: bool,
    active_turn_id: Option<String>,
    recent_items: Vec<Value>,
    usage: Option<Value>,
    subscribers: HashMap<String, SubscriptionMode>,
    events: broadcast::Sender<Envelope<Value>>,
}

impl ThreadActor {
    pub fn spawn(
        thread_id: String,
        store: EventStore,
        snapshots: SnapshotStore,
        approvals: ApprovalRegistry,
        leases: LeaseManager,
        upstream: Arc<dyn AppServerTransport>,
        metrics: Arc<BrokerMetrics>,
    ) -> ThreadActorHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(512);
        let actor = Self {
            thread_id: thread_id.clone(),
            store,
            snapshots,
            approvals,
            leases,
            upstream,
            metrics,
            status: ThreadRuntimeStatus::Idle,
            loaded: false,
            active_turn_id: None,
            recent_items: Vec::new(),
            usage: None,
            subscribers: HashMap::new(),
            events: events.clone(),
        };
        tokio::spawn(actor.run(rx));
        ThreadActorHandle {
            thread_id,
            tx,
            events,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ThreadActorMessage>) {
        let _ = self.store.ensure_thread(&self.thread_id, None, self.status);
        while let Some(message) = rx.recv().await {
            match message {
                ThreadActorMessage::AttachClient {
                    device_id,
                    mode,
                    last_ack_seq,
                    reply,
                } => {
                    self.subscribers.insert(device_id.clone(), mode);
                    let _ = reply.send(self.attach(&device_id, last_ack_seq).await);
                }
                ThreadActorMessage::DetachClient { device_id } => {
                    self.subscribers.remove(&device_id);
                }
                ThreadActorMessage::ClientCommand {
                    device_id,
                    request,
                    reply,
                } => {
                    let _ = reply.send(self.handle_command(&device_id, request).await);
                }
                ThreadActorMessage::Ack {
                    device_id,
                    through_seq,
                } => {
                    let _ = self.store.set_ack(&device_id, &self.thread_id, through_seq);
                }
                ThreadActorMessage::UpstreamEvent {
                    epoch,
                    method,
                    payload,
                } => {
                    self.ingest_upstream_event(epoch, method, payload).await;
                }
                ThreadActorMessage::UpstreamServerRequest {
                    epoch,
                    request_id,
                    method,
                    payload,
                } => {
                    self.ingest_server_request(epoch, request_id, method, payload)
                        .await;
                }
                ThreadActorMessage::UpstreamDisconnected { epoch, .. } => {
                    let orphaned = self.approvals.orphan_epoch(epoch).unwrap_or(0);
                    self.metrics
                        .orphaned_approvals_total
                        .fetch_add(orphaned, std::sync::atomic::Ordering::Relaxed);
                    self.status = ThreadRuntimeStatus::Indeterminate;
                    let _ = self.persist_projection(Some(epoch)).await;
                }
                ThreadActorMessage::RecoverOnStartup => {
                    self.hydrate_from_store().await;
                    self.recover_on_startup(false).await;
                }
                ThreadActorMessage::RecoverAfterReconnect { epoch } => {
                    log::info!(
                        "thread {} recovering after reconnect epoch={epoch}",
                        self.thread_id
                    );
                    self.recover_on_startup(true).await;
                }
                ThreadActorMessage::CreateSnapshot => {
                    let _ = self.persist_snapshot().await;
                }
                ThreadActorMessage::Shutdown => break,
            }
        }
    }

    async fn attach(
        &mut self,
        _device_id: &str,
        last_ack_seq: u64,
    ) -> Result<AttachResult, RemoteError> {
        // Ensure local projection is hydrated before advertising a snapshot.
        if self.recent_items.is_empty() {
            self.hydrate_from_store().await;
        }
        let latest_seq = self.store.latest_seq(&self.thread_id).unwrap_or(0);
        let pending = self
            .approvals
            .list_pending_for_thread(&self.thread_id)
            .unwrap_or_default();
        let lease = self.leases.current(&self.thread_id).unwrap_or(None);
        let snapshot = self
            .snapshots
            .latest_snapshot(&self.thread_id)
            .ok()
            .flatten()
            .filter(|snap| !snap.items.is_empty() || snap.snapshot_seq > 0)
            .unwrap_or_else(|| self.current_snapshot(latest_seq, pending.clone(), lease.clone()));
        // Never claim a snapshot_seq beyond the projection we can actually serve.
        let mut snapshot = snapshot;
        if snapshot.items.is_empty() && !self.recent_items.is_empty() {
            snapshot.items = self.recent_items.clone();
            snapshot.usage = self.usage.clone();
            snapshot.status = self.status;
            snapshot.loaded = self.loaded;
        }
        // Cap snapshot seq to known latest thread_seq.
        if snapshot.snapshot_seq > latest_seq {
            snapshot.snapshot_seq = latest_seq;
        }
        let replay_from = if snapshot.snapshot_seq > last_ack_seq {
            snapshot.snapshot_seq
        } else {
            last_ack_seq
        };
        let replay = self
            .store
            .events_after(&self.thread_id, replay_from, 10_000)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.into_protocol())
            .collect::<Vec<_>>();
        self.metrics
            .replay_events_total
            .fetch_add(replay.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(AttachResult { snapshot, replay })
    }

    fn current_snapshot(
        &self,
        latest_seq: u64,
        pending: Vec<vellum_remote_protocol::PendingApproval>,
        lease: Option<vellum_remote_protocol::WriterLeaseView>,
    ) -> ThreadSnapshot {
        let mut snapshot = SnapshotStore::build_fallback_snapshot(
            &self.thread_id,
            latest_seq,
            self.status,
            self.loaded,
            pending,
            lease,
        );
        if let Some(turn_id) = &self.active_turn_id {
            snapshot.active_turn = Some(vellum_remote_protocol::ActiveTurnView {
                turn_id: turn_id.clone(),
                state: self.status,
                started_at: None,
            });
        }
        snapshot.items = self.recent_items.clone();
        snapshot.usage = self.usage.clone();
        snapshot
    }

    async fn handle_command(
        &mut self,
        device_id: &str,
        request: ThreadCommandRequest,
    ) -> ThreadCommandResult {
        if request.idempotency_key.trim().is_empty() {
            return failed(
                request.idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::CommandConflict,
                    "idempotencyKey is required",
                    false,
                ),
            );
        }
        let kind = command_kind(&request.command);
        let command_json = serde_json::to_value(&request.command).unwrap_or(Value::Null);

        let command_sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(
                serde_json::to_vec(&command_json).unwrap_or_default(),
            ))
        };
        if let Ok(Some((state, result, error, stored_kind, stored_device, stored_sha))) =
            self.store.get_command(&request.idempotency_key)
        {
            if stored_device.as_deref() != Some(device_id)
                || stored_kind.as_deref() != Some(kind)
                || stored_sha.as_deref().is_some_and(|sha| sha != command_sha)
            {
                return failed(
                    request.idempotency_key,
                    RemoteError::new(
                        RemoteErrorCode::CommandConflict,
                        "idempotency key reused with different command identity",
                        false,
                    ),
                );
            }
            self.metrics
                .command_idempotency_hits_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return match state.as_str() {
                "processing" => processing(request.idempotency_key),
                "completed" => completed(request.idempotency_key, result.unwrap_or(Value::Null)),
                "failed" => ThreadCommandResult {
                    idempotency_key: request.idempotency_key,
                    status: CommandStatus::Failed,
                    result: Value::Null,
                    error: error.and_then(|v| serde_json::from_value(v).ok()),
                },
                "indeterminate" => indeterminate(request.idempotency_key),
                other => failed(
                    request.idempotency_key,
                    RemoteError::new(
                        RemoteErrorCode::CommandConflict,
                        format!("unsupported stored command state: {other}"),
                        false,
                    ),
                ),
            };
        }

        if let Err(error) = self.store.store_command(
            &request.idempotency_key,
            device_id,
            request
                .thread_id
                .as_deref()
                .or(Some(self.thread_id.as_str())),
            kind,
            &command_json,
            "processing",
            None,
            None,
        ) {
            return failed(
                request.idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::InternalError,
                    format!("failed to persist command intent: {error}"),
                    true,
                ),
            );
        }

        if requires_writer(&request.command) {
            let lease = self.leases.current(&self.thread_id).ok().flatten();
            let allowed = match (&request.expected_writer_lease_id, lease) {
                (Some(expected), Some(current)) => {
                    expected == &current.lease_id && current.holder_device_id == device_id
                }
                (None, Some(current)) => current.holder_device_id == device_id,
                _ => false,
            };
            if !allowed {
                let result = lease_required(request.idempotency_key.clone());
                let _ = self.store.store_command(
                    &request.idempotency_key,
                    device_id,
                    Some(&self.thread_id),
                    kind,
                    &command_json,
                    "failed",
                    None,
                    result
                        .error
                        .as_ref()
                        .and_then(|e| serde_json::to_value(e).ok())
                        .as_ref(),
                );
                return result;
            }
        }

        let result = match request.command.clone() {
            ThreadCommand::ThreadStart { .. } => failed(
                request.idempotency_key.clone(),
                RemoteError::new(
                    RemoteErrorCode::InternalError,
                    "thread.start must be handled by broker coordinator",
                    false,
                ),
            ),
            ThreadCommand::ThreadResume { thread_id } => {
                if thread_id != self.thread_id {
                    failed(
                        request.idempotency_key.clone(),
                        RemoteError::new(
                            RemoteErrorCode::ThreadNotFound,
                            "thread.resume target does not match actor thread",
                            false,
                        ),
                    )
                } else {
                    match self
                        .upstream
                        .call("thread/resume", json!({"threadId": thread_id}))
                        .await
                    {
                        Ok(value) => {
                            self.loaded = true;
                            accepted(request.idempotency_key.clone(), value)
                        }
                        Err(error) => failed(
                            request.idempotency_key.clone(),
                            RemoteError::new(
                                RemoteErrorCode::UpstreamUnavailable,
                                error.to_string(),
                                true,
                            ),
                        ),
                    }
                }
            }
            ThreadCommand::TurnStart { input, settings } => {
                let mut params = json!({"threadId": self.thread_id, "input": input});
                if let Some(model) = settings.model {
                    params["model"] = json!(model);
                }
                if let Some(policy) = settings.approval_policy {
                    params["approvalPolicy"] = json!(policy);
                }
                match self.upstream.call("turn/start", params).await {
                    Ok(value) => {
                        self.status = ThreadRuntimeStatus::Running;
                        if let Some(turn_id) = extract_turn_id(&value) {
                            self.active_turn_id = Some(turn_id);
                        }
                        let _ = self.persist_projection(Some(self.upstream.epoch())).await;
                        accepted(request.idempotency_key.clone(), value)
                    }
                    Err(error) => {
                        let (state, remote_error) = classify_upstream_command_error(&error, true);
                        // store state later via generic path; encode via failed/indeterminate
                        if state == "indeterminate" {
                            indeterminate(request.idempotency_key.clone())
                        } else {
                            failed(request.idempotency_key.clone(), remote_error)
                        }
                    }
                }
            }
            ThreadCommand::TurnSteer {
                expected_turn_id,
                client_user_message_id,
                input,
            } => {
                match self
                    .upstream
                    .call(
                        "turn/steer",
                        json!({
                            "threadId": self.thread_id,
                            "expectedTurnId": expected_turn_id,
                            "clientUserMessageId": client_user_message_id,
                            "input": input
                        }),
                    )
                    .await
                {
                    Ok(value) => accepted(request.idempotency_key.clone(), value),
                    Err(error) => failed(
                        request.idempotency_key.clone(),
                        RemoteError::new(
                            RemoteErrorCode::UpstreamUnavailable,
                            error.to_string(),
                            true,
                        ),
                    ),
                }
            }
            ThreadCommand::TurnInterrupt { turn_id } => {
                match self
                    .upstream
                    .call(
                        "turn/interrupt",
                        json!({"threadId": self.thread_id, "turnId": turn_id}),
                    )
                    .await
                {
                    Ok(value) => {
                        self.status = ThreadRuntimeStatus::Interrupted;
                        let _ = self.persist_projection(None).await;
                        accepted(request.idempotency_key.clone(), value)
                    }
                    Err(error) => {
                        let (state, remote_error) = classify_upstream_command_error(&error, true);
                        if state == "indeterminate" {
                            indeterminate(request.idempotency_key.clone())
                        } else {
                            failed(request.idempotency_key.clone(), remote_error)
                        }
                    }
                }
            }
            ThreadCommand::ApprovalRespond {
                approval_token,
                decision,
            } => {
                self.respond_approval(&request.idempotency_key, &approval_token, decision)
                    .await
            }
            ThreadCommand::WriterAcquire => match self.leases.acquire(&self.thread_id, device_id) {
                Ok(Ok(lease)) => accepted(
                    request.idempotency_key.clone(),
                    serde_json::to_value(lease).unwrap_or(Value::Null),
                ),
                Ok(Err(holder)) => failed(
                    request.idempotency_key.clone(),
                    RemoteError {
                        code: RemoteErrorCode::WriterLeaseConflict,
                        message: "writer lease held by another device".into(),
                        retryable: true,
                        details: serde_json::to_value(holder).unwrap_or(Value::Null),
                    },
                ),
                Err(error) => failed(
                    request.idempotency_key.clone(),
                    RemoteError::new(RemoteErrorCode::InternalError, error.to_string(), true),
                ),
            },
            ThreadCommand::WriterRelease { lease_id } => {
                let ok = self
                    .leases
                    .release(&self.thread_id, device_id, &lease_id)
                    .unwrap_or(false);
                accepted(request.idempotency_key.clone(), json!({"released": ok}))
            }
            ThreadCommand::ThreadArchive => {
                match self
                    .upstream
                    .call("thread/archive", json!({"threadId": self.thread_id}))
                    .await
                {
                    Ok(value) => accepted(request.idempotency_key.clone(), value),
                    Err(error) => failed(
                        request.idempotency_key.clone(),
                        RemoteError::new(
                            RemoteErrorCode::UpstreamUnavailable,
                            error.to_string(),
                            true,
                        ),
                    ),
                }
            }
        };

        let state = match result.status {
            CommandStatus::Accepted | CommandStatus::Completed => "completed",
            CommandStatus::Processing => "processing",
            CommandStatus::Failed => "failed",
            CommandStatus::Indeterminate => "indeterminate",
        };
        let _ = self.store.store_command(
            &request.idempotency_key,
            device_id,
            Some(&self.thread_id),
            kind,
            &command_json,
            state,
            Some(&result.result),
            result
                .error
                .as_ref()
                .and_then(|e| serde_json::to_value(e).ok())
                .as_ref(),
        );
        let envelope = Envelope::new(
            message_type::THREAD_COMMAND_RESULT,
            Ulid::new().to_string(),
            serde_json::to_value(&result).unwrap_or(Value::Null),
        );
        let _ = self.events.send(envelope);
        result
    }

    async fn respond_approval(
        &mut self,
        idempotency_key: &str,
        approval_token: &str,
        decision: ApprovalDecision,
    ) -> ThreadCommandResult {
        let stored = match self.approvals.get(approval_token) {
            Ok(Some(v)) => v,
            Ok(None) => {
                return failed(
                    idempotency_key,
                    RemoteError::new(
                        RemoteErrorCode::ApprovalOrphaned,
                        "approval token not found",
                        false,
                    ),
                )
            }
            Err(error) => {
                return failed(
                    idempotency_key,
                    RemoteError::new(RemoteErrorCode::InternalError, error.to_string(), true),
                )
            }
        };
        if stored.upstream_epoch != self.upstream.epoch() {
            return failed(
                idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::StaleUpstreamEpoch,
                    "approval belongs to a previous upstream epoch",
                    false,
                ),
            );
        }
        let response = json!({"decision": decision});
        if let Err(error) = self
            .upstream
            .respond(stored.upstream_request_id, response.clone())
            .await
        {
            let (state, remote_error) = classify_upstream_command_error(&error, true);
            return if state == "indeterminate" {
                indeterminate(idempotency_key)
            } else {
                failed(idempotency_key, remote_error)
            };
        }
        let _ = self.approvals.resolve(approval_token, decision, &response);
        completed(idempotency_key, json!({"approvalToken": approval_token}))
    }

    async fn hydrate_from_store(&mut self) {
        if let Ok(Some((status, loaded, active_turn_id, _))) =
            self.store.thread_projection(&self.thread_id)
        {
            self.status = status;
            self.loaded = loaded;
            self.active_turn_id = active_turn_id;
        }
        if let Ok(Some(snapshot)) = self.snapshots.latest_snapshot(&self.thread_id) {
            self.status = snapshot.status;
            self.loaded = snapshot.loaded;
            self.active_turn_id = snapshot.active_turn.map(|turn| turn.turn_id);
            self.usage = snapshot.usage;
        }
        // Rebuild from the durable event log even when an older snapshot is
        // present. Legacy snapshots stored payload-only wrappers and cannot
        // restore timestamps, sequence numbers, turn durations, or the exact
        // typed item timeline after a Desktop reconnect.
        self.recent_items.clear();
        if let Ok(events) = self.store.session_timeline_events(&self.thread_id) {
            for event in events {
                self.project_event(&event.method, &event.payload, event.turn_id.clone());
                self.remember_event(&event.into_protocol());
            }
        }
    }

    fn project_event(&mut self, method: &str, payload: &Value, turn_id: Option<String>) {
        match method {
            "turn/started" => {
                self.status = ThreadRuntimeStatus::Running;
                self.active_turn_id = turn_id.clone();
            }
            "turn/completed" => {
                let turn_status = payload
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("status").and_then(Value::as_str))
                    .unwrap_or("completed");
                self.status = match turn_status {
                    "failed" => ThreadRuntimeStatus::Failed,
                    "interrupted" => ThreadRuntimeStatus::Interrupted,
                    "inProgress" => ThreadRuntimeStatus::Running,
                    _ => ThreadRuntimeStatus::Completed,
                };
                if !matches!(self.status, ThreadRuntimeStatus::Running) {
                    self.active_turn_id = None;
                }
            }
            "thread/status/changed" => {
                if let Some(status) = normalize_thread_status(payload) {
                    self.status = status;
                }
            }
            _ => {}
        }
        if method.contains("usage") || payload.get("usage").is_some() {
            self.usage = payload.get("usage").cloned().or(Some(payload.clone()));
        }
    }

    fn remember_event(&mut self, event: &vellum_remote_protocol::ThreadEvent) {
        if matches!(
            event.method.as_str(),
            "turn/started" | "turn/completed" | "item/started" | "item/completed"
        ) {
            self.recent_items
                .push(serde_json::to_value(event).unwrap_or(Value::Null));
        }
    }

    async fn recover_on_startup(&mut self, force_resume: bool) {
        // Only resume threads that still need an active upstream subscription.
        // force_resume still respects active/observer need so idle completed
        // actors are not bulk-resumed after every reconnect.
        let should_resume = self.loaded
            || matches!(
                self.status,
                ThreadRuntimeStatus::Running
                    | ThreadRuntimeStatus::WaitingForApproval
                    | ThreadRuntimeStatus::Indeterminate
                    | ThreadRuntimeStatus::Loading
                    | ThreadRuntimeStatus::Orphaned
            )
            || self.active_turn_id.is_some()
            || (force_resume && !self.subscribers.is_empty());
        let params = json!({ "threadId": self.thread_id });
        if should_resume {
            match self.upstream.call("thread/resume", params.clone()).await {
                Ok(value) => {
                    self.loaded = true;
                    if let Some(status) =
                        normalize_thread_status(value.get("thread").unwrap_or(&value))
                    {
                        self.status = status;
                    }
                    if let Some(turn) = value
                        .pointer("/thread/activeTurnId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| extract_turn_id(&value))
                    {
                        self.active_turn_id = Some(turn);
                    }
                    let _ = self.persist_projection(None).await;
                    return;
                }
                Err(error) => {
                    log::warn!(
                        "thread/resume failed for {}: {error}; trying thread/read",
                        self.thread_id
                    );
                }
            }
        }

        // thread/read is stored metadata only; success does not mean loaded.
        if let Ok(value) = self.upstream.call("thread/read", params).await {
            if let Some(status) = normalize_thread_status(value.get("thread").unwrap_or(&value)) {
                self.status = status;
            }
            if matches!(self.status, ThreadRuntimeStatus::StoredNotLoaded) {
                self.loaded = false;
            }
            let _ = self.persist_projection(None).await;
        } else if should_resume {
            self.status = ThreadRuntimeStatus::Indeterminate;
            let _ = self.persist_projection(None).await;
        }
    }

    async fn persist_projection(
        &self,
        epoch: Option<u64>,
    ) -> Result<(), crate::event_store::StoreError> {
        self.store.update_thread_projection(
            &self.thread_id,
            &ThreadProjectionUpdate {
                status: self.status,
                loaded: self.loaded,
                active_turn_id: Some(self.active_turn_id.clone()),
                upstream_epoch: epoch.or_else(|| {
                    let current = self.upstream.epoch();
                    if current == 0 {
                        None
                    } else {
                        Some(current)
                    }
                }),
            },
        )
    }

    async fn ingest_upstream_event(&mut self, epoch: u64, method: String, payload: Value) {
        let turn_id = extract_turn_id(&payload);
        let item_id = extract_item_id(&payload);
        match method.as_str() {
            "turn/started" => {
                self.status = ThreadRuntimeStatus::Running;
                self.active_turn_id = turn_id.clone();
            }
            "turn/completed" => {
                // Official payload uses turn.status object/string, not only method name.
                let turn_status = payload
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("status").and_then(Value::as_str))
                    .unwrap_or("completed");
                self.status = match turn_status {
                    "failed" => ThreadRuntimeStatus::Failed,
                    "interrupted" => ThreadRuntimeStatus::Interrupted,
                    "inProgress" => ThreadRuntimeStatus::Running,
                    _ => ThreadRuntimeStatus::Completed,
                };
                if !matches!(self.status, ThreadRuntimeStatus::Running) {
                    self.active_turn_id = None;
                }
            }
            "thread/status/changed" => {
                if let Some(status) = normalize_thread_status(&payload) {
                    self.status = status;
                }
            }
            _ => {}
        }
        if method.contains("usage") || payload.get("usage").is_some() {
            self.usage = payload.get("usage").cloned().or(Some(payload.clone()));
        }

        let clear_turn = matches!(
            self.status,
            ThreadRuntimeStatus::Completed
                | ThreadRuntimeStatus::Failed
                | ThreadRuntimeStatus::Interrupted
                | ThreadRuntimeStatus::Idle
        ) && self.active_turn_id.is_none();
        let projection = ThreadProjectionUpdate {
            status: self.status,
            loaded: self.loaded,
            active_turn_id: if clear_turn {
                Some(None)
            } else if self.active_turn_id.is_some() {
                Some(self.active_turn_id.clone())
            } else {
                None
            },
            upstream_epoch: Some(epoch),
        };
        match self.store.append_event(
            &self.thread_id,
            epoch,
            &method,
            payload,
            turn_id.as_deref(),
            item_id.as_deref(),
            Utc::now(),
            &projection,
        ) {
            Ok(stored) => {
                self.metrics
                    .event_seq
                    .store(stored.seq, std::sync::atomic::Ordering::Relaxed);
                let event = stored.into_protocol();
                self.remember_event(&event);
                let envelope = Envelope::new(
                    message_type::THREAD_EVENT,
                    Ulid::new().to_string(),
                    serde_json::to_value(&event).unwrap_or(Value::Null),
                );
                let _ = self.events.send(envelope);
                if matches!(
                    self.status,
                    ThreadRuntimeStatus::Completed | ThreadRuntimeStatus::WaitingForApproval
                ) || event.seq % 250 == 0
                {
                    let _ = self.persist_snapshot().await;
                }
            }
            Err(error) => log::error!("failed to persist upstream event: {error}"),
        }
    }

    async fn ingest_server_request(
        &mut self,
        epoch: u64,
        request_id: JsonRpcId,
        method: String,
        payload: Value,
    ) {
        match method.as_str() {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {}
            other => {
                log::warn!("rejecting unsupported server request method: {other}");
                let _ = self
                    .upstream
                    .respond(
                        request_id,
                        json!({"error": format!("unsupported server request: {other}")}),
                    )
                    .await;
                return;
            }
        }
        match self.approvals.register(
            epoch,
            request_id,
            method,
            payload,
            Some(self.thread_id.clone()),
            None,
            None,
        ) {
            Ok(stored) => {
                self.status = ThreadRuntimeStatus::WaitingForApproval;
                self.metrics
                    .pending_approvals
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = self.persist_projection(Some(epoch)).await;
                let envelope = Envelope::new(
                    "approval.pending",
                    Ulid::new().to_string(),
                    serde_json::to_value(&stored.approval).unwrap_or(Value::Null),
                );
                let _ = self.events.send(envelope);
                let _ = self.persist_snapshot().await;
            }
            Err(error) => log::error!("failed to register approval: {error}"),
        }
    }

    async fn persist_snapshot(&self) -> Result<(), crate::event_store::StoreError> {
        let latest_seq = self.store.latest_seq(&self.thread_id)?;
        let pending = self.approvals.list_pending_for_thread(&self.thread_id)?;
        let lease = self.leases.current(&self.thread_id)?;
        let snapshot = self.current_snapshot(latest_seq, pending, lease);
        self.snapshots.save_snapshot(&snapshot)?;
        self.metrics
            .snapshot_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

pub fn parse_created_thread_id(value: &Value) -> Option<String> {
    extract_thread_id_from_result(value)
}

fn extract_item_id(payload: &Value) -> Option<String> {
    payload
        .get("itemId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn classify_upstream_command_error(
    error: &crate::app_server::transport::AppServerError,
    side_effectful: bool,
) -> (&'static str, RemoteError) {
    use crate::app_server::transport::AppServerError;
    match error {
        AppServerError::NotReady | AppServerError::Incompatible(_) => (
            "failed",
            RemoteError::new(
                RemoteErrorCode::UpstreamUnavailable,
                error.to_string(),
                true,
            ),
        ),
        AppServerError::Timeout | AppServerError::Transport(_) if side_effectful => (
            "indeterminate",
            RemoteError::new(
                RemoteErrorCode::UpstreamUnavailable,
                format!("indeterminate after possible upstream accept: {error}"),
                true,
            ),
        ),
        _ => (
            "failed",
            RemoteError::new(
                RemoteErrorCode::UpstreamUnavailable,
                error.to_string(),
                true,
            ),
        ),
    }
}

fn normalize_thread_status(payload: &Value) -> Option<ThreadRuntimeStatus> {
    // Official ThreadStatus is an object: { "type": "idle" | "active" | ... }
    if let Some(status_type) = payload
        .pointer("/status/type")
        .and_then(Value::as_str)
        .or_else(|| payload.get("type").and_then(Value::as_str))
    {
        return Some(match status_type {
            "active" | "running" => ThreadRuntimeStatus::Running,
            "idle" => ThreadRuntimeStatus::Idle,
            "notLoaded" => ThreadRuntimeStatus::StoredNotLoaded,
            "systemError" | "failed" => ThreadRuntimeStatus::Failed,
            "interrupted" => ThreadRuntimeStatus::Interrupted,
            _ => return None,
        });
    }
    if let Some(status) = payload.get("status").and_then(Value::as_str) {
        return Some(match status {
            "running" | "active" => ThreadRuntimeStatus::Running,
            "idle" => ThreadRuntimeStatus::Idle,
            "failed" => ThreadRuntimeStatus::Failed,
            "interrupted" => ThreadRuntimeStatus::Interrupted,
            _ => return None,
        });
    }
    None
}
