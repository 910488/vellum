//! Broker-level upstream dispatcher.
//!
//! One transport subscription fans out notifications/server-requests to the
//! correct thread actor by `threadId`, using the connection-captured epoch.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;

use super::jsonrpc::JsonRpcId;
use super::transport::{AppServerTransport, UpstreamMessage};
use crate::thread_actor::{ThreadActorHandle, ThreadActorMessage};

pub struct UpstreamSupervisor {
    transport: Arc<dyn AppServerTransport>,
    actors: Arc<RwLock<HashMap<String, ThreadActorHandle>>>,
}

impl UpstreamSupervisor {
    pub fn spawn(
        transport: Arc<dyn AppServerTransport>,
        actors: Arc<RwLock<HashMap<String, ThreadActorHandle>>>,
    ) {
        let supervisor = Self { transport, actors };
        tokio::spawn(async move {
            supervisor.run().await;
        });
    }

    async fn run(self) {
        let mut rx = self.transport.subscribe();
        loop {
            match rx.recv().await {
                Ok(message) => self.dispatch(message).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // Durable path must not silently drop forever; force reconnect recovery.
                    log::error!(
                        "upstream event channel lagged by {skipped}; marking actors for reconnect recovery"
                    );
                    let epoch = self.transport.epoch();
                    let actors = self.actors.read().await;
                    for actor in actors.values() {
                        let _ = actor
                            .sender()
                            .send(ThreadActorMessage::UpstreamDisconnected {
                                epoch,
                                reason: format!("upstream event lag skipped={skipped}"),
                            });
                        let _ = actor
                            .sender()
                            .send(ThreadActorMessage::RecoverAfterReconnect { epoch });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    async fn dispatch(&self, message: UpstreamMessage) {
        match message {
            UpstreamMessage::Notification {
                epoch,
                method,
                params,
            } => {
                if let Some(thread_id) = extract_thread_id(&params) {
                    if let Some(actor) = self.actors.read().await.get(&thread_id).cloned() {
                        let _ = actor.sender().send(ThreadActorMessage::UpstreamEvent {
                            epoch,
                            method,
                            payload: params,
                        });
                    } else {
                        log::debug!(
                            "dropping unroutable upstream notification {method} for missing thread {thread_id}"
                        );
                    }
                } else {
                    log::debug!("control-plane upstream notification: {method}");
                }
            }
            UpstreamMessage::ServerRequest {
                epoch,
                id,
                method,
                params,
            } => {
                if let Some(thread_id) = extract_thread_id(&params) {
                    if let Some(actor) = self.actors.read().await.get(&thread_id).cloned() {
                        let _ = actor
                            .sender()
                            .send(ThreadActorMessage::UpstreamServerRequest {
                                epoch,
                                request_id: id,
                                method,
                                payload: params,
                            });
                    } else {
                        log::warn!(
                            "no actor for server request {method} thread={thread_id}; rejecting unsupported"
                        );
                        let _ = self
                            .transport
                            .respond(
                                id,
                                serde_json::json!({
                                    "error": "thread actor not loaded"
                                }),
                            )
                            .await;
                    }
                } else {
                    log::warn!("server request without thread id: {method}");
                }
            }
            UpstreamMessage::Disconnected { epoch, reason } => {
                let actors = self.actors.read().await;
                for actor in actors.values() {
                    let _ = actor
                        .sender()
                        .send(ThreadActorMessage::UpstreamDisconnected {
                            epoch,
                            reason: reason.clone(),
                        });
                }
            }
            UpstreamMessage::Ready { epoch, identity } => {
                log::info!(
                    "upstream ready epoch={epoch} version={} — recovering actors",
                    identity.version
                );
                let actors = self.actors.read().await;
                for actor in actors.values() {
                    let _ = actor
                        .sender()
                        .send(ThreadActorMessage::RecoverAfterReconnect { epoch });
                }
            }
        }
    }
}

pub fn extract_thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub fn extract_turn_id(value: &Value) -> Option<String> {
    value
        .get("turnId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub fn extract_thread_id_from_result(value: &Value) -> Option<String> {
    extract_thread_id(value).or_else(|| value.get("result").and_then(extract_thread_id))
}

#[allow(dead_code)]
pub fn json_rpc_id_debug(id: &JsonRpcId) -> String {
    match id {
        JsonRpcId::Number(n) => n.to_string(),
        JsonRpcId::String(s) => s.clone(),
    }
}
