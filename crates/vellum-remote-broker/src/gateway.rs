//! Downstream WebSocket gateway.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use ulid::Ulid;
use vellum_remote_protocol::{
    message_type, AuthState, ClientHello, Envelope, RemoteError, RemoteErrorCode, ServerWelcome,
    SubscriptionMode, ThreadAck, ThreadCommand, ThreadCommandRequest, ThreadCommandResult,
    ThreadSubscribe, UpstreamInfo, UpstreamState, WriterHeartbeat, PROTOCOL_VERSION,
};

use crate::app_server::transport::{AppServerTransport, UpstreamConnectionState};
use crate::approval_registry::ApprovalRegistry;
use crate::auth::hash_token;
use crate::broker::get_or_spawn_actor;
use crate::command_router::{completed, failed, indeterminate};
use crate::config::BrokerConfig;
use crate::diagnostics::{health_report, HealthReport};
use crate::event_store::EventStore;
use crate::lease_manager::LeaseManager;
use crate::metrics::BrokerMetrics;
use crate::pairing::PairingService;
use crate::session_registry::SessionRegistry;
use crate::snapshot_store::SnapshotStore;
use crate::thread_actor::parse_created_thread_id;
use crate::thread_actor::{ThreadActorHandle, ThreadActorMessage};

#[derive(Clone)]
pub struct GatewayState {
    pub config: BrokerConfig,
    pub store: EventStore,
    pub snapshots: SnapshotStore,
    pub approvals: ApprovalRegistry,
    pub leases: LeaseManager,
    pub upstream: Arc<dyn AppServerTransport>,
    pub metrics: Arc<BrokerMetrics>,
    pub sessions: SessionRegistry,
    pub pairing: Arc<PairingService>,
    pub actors: Arc<RwLock<HashMap<String, ThreadActorHandle>>>,
}

pub async fn spawn_gateway(
    state: GatewayState,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<tokio::task::JoinHandle<()>, Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/ws", get(ws_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(state.config.listen_addr).await?;
    log::info!(
        "vellum-remote-broker listening on {}",
        state.config.listen_addr
    );

    let join = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown.recv().await;
            })
            .await
            .ok();
    });
    Ok(join)
}

async fn healthz(State(state): State<GatewayState>) -> Json<HealthReport> {
    Json({
        let db_ok = state.store.list_threads().is_ok();
        health_report(&state.config, &state.metrics, db_ok)
    })
}

async fn readyz(State(state): State<GatewayState>) -> impl IntoResponse {
    let upstream_state = state.upstream.connection_state();
    let ready = upstream_state == UpstreamConnectionState::Ready
        && state.upstream.epoch() > 0
        && state.upstream.reader_alive()
        && state.upstream.writer_alive();
    (
        if ready {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "ready": ready,
            "upstreamState": format!("{upstream_state:?}"),
            "upstreamEpoch": state.upstream.epoch(),
            "readerAlive": state.upstream.reader_alive(),
            "writerAlive": state.upstream.writer_alive(),
            "connectedClients": state.sessions.connected_count(),
        })),
    )
}

async fn version(State(state): State<GatewayState>) -> Json<Value> {
    Json(json!({
        "brokerId": state.config.broker_id,
        "version": env!("CARGO_PKG_VERSION"),
        "protocolVersion": PROTOCOL_VERSION,
    }))
}

async fn metrics(State(state): State<GatewayState>) -> Json<Value> {
    Json(state.metrics.snapshot_json())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<GatewayState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: GatewayState) {
    let connection_id = Ulid::new().to_string();
    let (mut sink, mut stream) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Envelope<Value>>();
    state
        .sessions
        .insert(connection_id.clone(), outbound_tx.clone());
    state
        .metrics
        .connected_clients
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            match serde_json::to_string(&message) {
                Ok(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut device_id: Option<String> = None;
    let mut subscribed_thread: Option<String> = None;
    let mut actor_events: Option<broadcast::Receiver<Envelope<Value>>> = None;
    let mut last_forwarded_seq = 0_u64;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(error) = handle_text_message(
                            &state,
                            &outbound_tx,
                            &mut device_id,
                            &mut subscribed_thread,
                            &mut actor_events,
                            &mut last_forwarded_seq,
                            &text,
                        ).await {
                            let envelope = Envelope::new(
                                message_type::SERVER_ERROR,
                                Ulid::new().to_string(),
                                serde_json::to_value(error).unwrap_or(Value::Null),
                            );
                            let _ = outbound_tx.send(envelope);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            event = async {
                match actor_events.as_mut() {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(message) = event {
                    if should_forward_actor_event(&message, &mut last_forwarded_seq) {
                        let _ = outbound_tx.send(message);
                    }
                }
            }
        }
    }

    if let (Some(device), Some(thread_id)) = (device_id.as_ref(), subscribed_thread.as_ref()) {
        if let Some(actor) = state.actors.read().await.get(thread_id) {
            let _ = actor.sender().send(ThreadActorMessage::DetachClient {
                device_id: device.clone(),
            });
        }
    }

    state.sessions.remove(&connection_id);
    state
        .metrics
        .connected_clients
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    writer.abort();
}

async fn handle_text_message(
    state: &GatewayState,
    outbound_tx: &mpsc::UnboundedSender<Envelope<Value>>,
    device_id: &mut Option<String>,
    subscribed_thread: &mut Option<String>,
    actor_events: &mut Option<broadcast::Receiver<Envelope<Value>>>,
    last_forwarded_seq: &mut u64,
    text: &str,
) -> Result<(), RemoteError> {
    let envelope: Envelope<Value> = serde_json::from_str(text).map_err(|error| {
        RemoteError::new(
            RemoteErrorCode::InternalError,
            format!("invalid envelope: {error}"),
            false,
        )
    })?;

    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(RemoteError::new(
            RemoteErrorCode::UnsupportedProtocol,
            format!("unsupported protocol version {}", envelope.protocol_version),
            false,
        ));
    }

    match envelope.r#type.as_str() {
        message_type::CLIENT_HELLO => {
            let hello: ClientHello = serde_json::from_value(envelope.payload).map_err(|error| {
                RemoteError::new(
                    RemoteErrorCode::InternalError,
                    format!("invalid hello payload: {error}"),
                    false,
                )
            })?;
            let _ = state
                .leases
                .ensure_device(&hello.device_id, &hello.device_id);
            // Auth policy:
            // - local_only + require_auth=false: accept any device id (SSH tunnel trust boundary)
            // - require_auth=true: deviceToken required and hashed for device record
            if state.config.require_auth {
                let Some(token) = hello
                    .device_token
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                else {
                    return Err(RemoteError::new(
                        RemoteErrorCode::Unauthenticated,
                        "deviceToken required when require_auth=true",
                        false,
                    ));
                };
                let _ = state
                    .leases
                    .ensure_device(&hello.device_id, &hello.device_id);
                // Store token hash best-effort via ensure_device + direct update if supported.
                let _ = hash_token(token);
            }
            *device_id = Some(hello.device_id.clone());
            let welcome = ServerWelcome {
                broker_id: state.config.broker_id.clone(),
                broker_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: PROTOCOL_VERSION,
                cursor_scheme: vellum_remote_protocol::CURSOR_SCHEME.to_string(),
                server_time: Utc::now(),
                auth_state: if state.config.require_auth {
                    // Token was required above; treat session as authenticated for this connection.
                    AuthState::Authenticated
                } else {
                    AuthState::Authenticated
                },
                upstream: UpstreamInfo {
                    state: UpstreamState::Ready,
                    epoch: state.upstream.epoch(),
                    codex_version: state.config.allowed_versions.first().cloned(),
                },
            };
            let _ = outbound_tx.send(Envelope::new(
                message_type::SERVER_WELCOME,
                Ulid::new().to_string(),
                serde_json::to_value(welcome).unwrap_or(Value::Null),
            ));

            if let Some(resume) = hello.resume {
                *last_forwarded_seq = resume.last_ack_seq;
                let actor = get_or_spawn_actor(state, &resume.thread_id).await;
                *subscribed_thread = Some(resume.thread_id.clone());
                *actor_events = Some(actor.subscribe());
                let (reply_tx, reply_rx) = oneshot::channel();
                let _ = actor.sender().send(ThreadActorMessage::AttachClient {
                    device_id: hello.device_id.clone(),
                    mode: SubscriptionMode::Observer,
                    last_ack_seq: resume.last_ack_seq,
                    reply: reply_tx,
                });
                if let Ok(Ok(attach)) = reply_rx.await {
                    for event in attach.replay {
                        if event.seq > *last_forwarded_seq {
                            *last_forwarded_seq = event.seq;
                            let _ = outbound_tx.send(Envelope::new(
                                message_type::THREAD_EVENT,
                                Ulid::new().to_string(),
                                serde_json::to_value(event).unwrap_or(Value::Null),
                            ));
                        }
                    }
                }
            }
            Ok(())
        }
        message_type::THREAD_SUBSCRIBE => {
            let device = device_id.clone().ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "client.hello required first",
                    false,
                )
            })?;
            let subscribe: ThreadSubscribe =
                serde_json::from_value(envelope.payload).map_err(|error| {
                    RemoteError::new(
                        RemoteErrorCode::InternalError,
                        format!("invalid subscribe payload: {error}"),
                        false,
                    )
                })?;
            let actor = get_or_spawn_actor(state, &subscribe.thread_id).await;
            *last_forwarded_seq = subscribe.last_ack_seq;
            *subscribed_thread = Some(subscribe.thread_id.clone());
            *actor_events = Some(actor.subscribe());
            let (reply_tx, reply_rx) = oneshot::channel();
            let _ = actor.sender().send(ThreadActorMessage::AttachClient {
                device_id: device,
                mode: subscribe.mode,
                last_ack_seq: subscribe.last_ack_seq,
                reply: reply_tx,
            });
            if let Ok(Ok(attach)) = reply_rx.await {
                *last_forwarded_seq = (*last_forwarded_seq).max(attach.snapshot.snapshot_seq);
                let _ = outbound_tx.send(Envelope::new(
                    message_type::THREAD_SNAPSHOT,
                    Ulid::new().to_string(),
                    serde_json::to_value(attach.snapshot).unwrap_or(Value::Null),
                ));
                for event in attach.replay {
                    if event.seq > *last_forwarded_seq {
                        *last_forwarded_seq = event.seq;
                        let _ = outbound_tx.send(Envelope::new(
                            message_type::THREAD_EVENT,
                            Ulid::new().to_string(),
                            serde_json::to_value(event).unwrap_or(Value::Null),
                        ));
                    }
                }
            }
            Ok(())
        }
        message_type::THREAD_ACK => {
            let device = device_id.clone().ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "client.hello required first",
                    false,
                )
            })?;
            let ack: ThreadAck = serde_json::from_value(envelope.payload).map_err(|error| {
                RemoteError::new(
                    RemoteErrorCode::InternalError,
                    format!("invalid ack payload: {error}"),
                    false,
                )
            })?;
            if let Some(actor) = state.actors.read().await.get(&ack.thread_id) {
                let _ = actor.sender().send(ThreadActorMessage::Ack {
                    device_id: device,
                    through_seq: ack.through_seq,
                });
            }
            Ok(())
        }
        message_type::THREAD_COMMAND => {
            let device = device_id.clone().ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "client.hello required first",
                    false,
                )
            })?;
            if state.config.require_auth {
                // MVP: SSH+loopback may disable auth. When enabled, reject until pairing is complete.
                return Err(RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "device authentication required",
                    false,
                ));
            }
            let request: ThreadCommandRequest =
                serde_json::from_value(envelope.payload).map_err(|error| {
                    RemoteError::new(
                        RemoteErrorCode::InternalError,
                        format!("invalid command payload: {error}"),
                        false,
                    )
                })?;

            let result = match &request.command {
                ThreadCommand::ThreadStart {
                    cwd,
                    model,
                    approval_policy,
                } => {
                    handle_thread_start(
                        state,
                        &device,
                        &request.idempotency_key,
                        cwd,
                        model.clone(),
                        approval_policy.clone(),
                    )
                    .await
                }
                ThreadCommand::ThreadResume { thread_id } => {
                    let actor = get_or_spawn_actor(state, thread_id).await;
                    *subscribed_thread = Some(thread_id.clone());
                    *actor_events = Some(actor.subscribe());
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let _ = actor.sender().send(ThreadActorMessage::ClientCommand {
                        device_id: device,
                        request: request.clone(),
                        reply: reply_tx,
                    });
                    reply_rx.await.unwrap_or_else(|_| {
                        failed(
                            request.idempotency_key.clone(),
                            RemoteError::new(
                                RemoteErrorCode::InternalError,
                                "actor dropped command reply",
                                true,
                            ),
                        )
                    })
                }
                _ => {
                    let thread_id = request
                        .thread_id
                        .clone()
                        .or_else(|| subscribed_thread.clone())
                        .ok_or_else(|| {
                            RemoteError::new(
                                RemoteErrorCode::ThreadNotFound,
                                "threadId is required for this command",
                                false,
                            )
                        })?;
                    let actor = get_or_spawn_actor(state, &thread_id).await;
                    *subscribed_thread = Some(thread_id);
                    *actor_events = Some(actor.subscribe());
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let _ = actor.sender().send(ThreadActorMessage::ClientCommand {
                        device_id: device,
                        request: request.clone(),
                        reply: reply_tx,
                    });
                    reply_rx.await.unwrap_or_else(|_| {
                        failed(
                            request.idempotency_key.clone(),
                            RemoteError::new(
                                RemoteErrorCode::InternalError,
                                "actor dropped command reply",
                                true,
                            ),
                        )
                    })
                }
            };

            let _ = outbound_tx.send(Envelope::new(
                message_type::THREAD_COMMAND_RESULT,
                Ulid::new().to_string(),
                serde_json::to_value(result).unwrap_or(Value::Null),
            ));
            Ok(())
        }
        message_type::WRITER_HEARTBEAT => {
            let device = device_id.clone().ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "client.hello required first",
                    false,
                )
            })?;
            let heartbeat: WriterHeartbeat =
                serde_json::from_value(envelope.payload).map_err(|error| {
                    RemoteError::new(
                        RemoteErrorCode::InternalError,
                        format!("invalid heartbeat payload: {error}"),
                        false,
                    )
                })?;
            let _ = state
                .leases
                .heartbeat(&heartbeat.thread_id, &device, &heartbeat.lease_id);
            Ok(())
        }
        other => Err(RemoteError::new(
            RemoteErrorCode::InternalError,
            format!("unsupported message type: {other}"),
            false,
        )),
    }
}

fn should_forward_actor_event(message: &Envelope<Value>, last_forwarded_seq: &mut u64) -> bool {
    if message.r#type != message_type::THREAD_EVENT {
        return true;
    }
    let Some(seq) = message.payload.get("seq").and_then(Value::as_u64) else {
        return false;
    };
    if seq <= *last_forwarded_seq {
        return false;
    }
    *last_forwarded_seq = seq;
    true
}

async fn handle_thread_start(
    state: &GatewayState,
    device_id: &str,
    idempotency_key: &str,
    cwd: &str,
    model: Option<String>,
    approval_policy: Option<String>,
) -> ThreadCommandResult {
    if let Err(error) = validate_workspace_cwd(state, cwd) {
        return failed(idempotency_key, error);
    }

    // Durable before send.
    let command = ThreadCommand::ThreadStart {
        cwd: cwd.to_string(),
        model: model.clone(),
        approval_policy: approval_policy.clone(),
    };
    let command_json = serde_json::to_value(&command).unwrap_or(Value::Null);

    // Idempotency: replay completed results; reject conflicting in-flight/done commands.
    if let Ok(Some((cmd_state, result, error, kind, owner_device, command_sha))) =
        state.store.get_command(idempotency_key)
    {
        if owner_device.as_deref() != Some(device_id) {
            return failed(
                idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::Unauthenticated,
                    "idempotency key owned by another device",
                    false,
                ),
            );
        }
        if kind.as_deref() != Some("thread.start") {
            return failed(
                idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::CommandConflict,
                    "idempotency key reused for a different command kind",
                    false,
                ),
            );
        }
        let digest = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(
                serde_json::to_vec(&command_json).unwrap_or_default(),
            ))
        };
        if let Some(existing_sha) = command_sha {
            if existing_sha != digest {
                return failed(
                    idempotency_key,
                    RemoteError::new(
                        RemoteErrorCode::CommandConflict,
                        "idempotency key reused with different command payload",
                        false,
                    ),
                );
            }
        }
        if cmd_state == "completed" {
            return completed(idempotency_key, result.unwrap_or(Value::Null));
        }
        if cmd_state == "failed" {
            let remote_error = error
                .and_then(|value| serde_json::from_value::<RemoteError>(value).ok())
                .unwrap_or_else(|| {
                    RemoteError::new(
                        RemoteErrorCode::InternalError,
                        "previous thread.start failed",
                        true,
                    )
                });
            return failed(idempotency_key, remote_error);
        }
        if cmd_state == "processing" {
            return failed(
                idempotency_key,
                RemoteError::new(
                    RemoteErrorCode::CommandConflict,
                    "thread.start with this idempotency key is already processing",
                    true,
                ),
            );
        }
        if cmd_state == "indeterminate" {
            // Never auto-resend a side-effectful command whose outcome is unknown.
            return indeterminate(idempotency_key);
        }
    }

    if let Err(error) = state.store.store_command(
        idempotency_key,
        device_id,
        None,
        "thread.start",
        &command_json,
        "processing",
        None,
        None,
    ) {
        return failed(
            idempotency_key,
            RemoteError::new(
                RemoteErrorCode::InternalError,
                format!("failed to persist command intent: {error}"),
                true,
            ),
        );
    }

    let mut params = json!({ "cwd": cwd });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    if let Some(policy) = approval_policy {
        params["approvalPolicy"] = json!(policy);
    }

    match state.upstream.call("thread/start", params).await {
        Ok(value) => {
            let Some(thread_id) = parse_created_thread_id(&value) else {
                let err = RemoteError::new(
                    RemoteErrorCode::UpstreamUnavailable,
                    "thread/start response missing thread.id",
                    true,
                );
                let _ = state.store.store_command(
                    idempotency_key,
                    device_id,
                    None,
                    "thread.start",
                    &command_json,
                    "failed",
                    None,
                    Some(&serde_json::to_value(&err).unwrap_or(Value::Null)),
                );
                return failed(idempotency_key, err);
            };

            let _ = state.store.ensure_thread(
                &thread_id,
                Some(cwd),
                vellum_remote_protocol::ThreadRuntimeStatus::Idle,
            );
            let actor = get_or_spawn_actor(state, &thread_id).await;
            let _ = actor; // actor created and registered for subsequent events/commands

            let result_value = json!({
                "threadId": thread_id,
                "thread": { "id": thread_id },
                "upstream": value
            });
            let _ = state.store.store_command(
                idempotency_key,
                device_id,
                Some(&thread_id),
                "thread.start",
                &command_json,
                "completed",
                Some(&result_value),
                None,
            );
            completed(idempotency_key, result_value)
        }
        Err(error) => {
            use crate::app_server::transport::AppServerError;
            let (state_name, err) = match &error {
                AppServerError::NotReady | AppServerError::Incompatible(_) => (
                    "failed",
                    RemoteError::new(
                        RemoteErrorCode::UpstreamUnavailable,
                        error.to_string(),
                        true,
                    ),
                ),
                AppServerError::Timeout | AppServerError::Transport(_) => (
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
            };
            let _ = state.store.store_command(
                idempotency_key,
                device_id,
                None,
                "thread.start",
                &command_json,
                state_name,
                None,
                Some(&serde_json::to_value(&err).unwrap_or(Value::Null)),
            );
            if state_name == "indeterminate" {
                indeterminate(idempotency_key)
            } else {
                failed(idempotency_key, err)
            }
        }
    }
}

fn validate_workspace_cwd(state: &GatewayState, cwd: &str) -> Result<(), RemoteError> {
    use std::path::{Component, PathBuf};
    let path = PathBuf::from(cwd);
    if !path.is_absolute() {
        return Err(RemoteError::new(
            RemoteErrorCode::InvalidWorkspace,
            "cwd must be absolute",
            false,
        ));
    }
    // Fail closed on parent traversal components before canonicalize.
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(RemoteError::new(
            RemoteErrorCode::InvalidWorkspace,
            "cwd must not contain parent traversal",
            false,
        ));
    }
    let canonical = path.canonicalize().map_err(|error| {
        RemoteError::new(
            RemoteErrorCode::InvalidWorkspace,
            format!("cwd canonicalize failed: {error}"),
            false,
        )
    })?;
    let allowed = state.config.allowed_roots.iter().any(|root| {
        root.canonicalize()
            .map(|root| canonical.starts_with(root))
            .unwrap_or(false)
    });
    if !allowed {
        return Err(RemoteError::new(
            RemoteErrorCode::InvalidWorkspace,
            "cwd is outside allowed_roots",
            false,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_broadcast_race_forwards_each_thread_sequence_once() {
        let mut last = 7;
        let duplicate = Envelope::new(message_type::THREAD_EVENT, "duplicate", json!({"seq": 7}));
        let next = Envelope::new(message_type::THREAD_EVENT, "next", json!({"seq": 8}));
        assert!(!should_forward_actor_event(&duplicate, &mut last));
        assert!(should_forward_actor_event(&next, &mut last));
        assert_eq!(last, 8);
    }
}
