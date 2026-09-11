//! In-process fake app-server adapter for tests only.
//! Production Broker must use `UnixWsAppServerTransport`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use super::jsonrpc::JsonRpcId;
use super::transport::{
    AppServerError, AppServerTransport, ServerIdentity, UpstreamConnectionState, UpstreamMessage,
};

#[derive(Clone)]
pub struct AppServerAdapter {
    epoch: Arc<AtomicU64>,
    next_id: Arc<AtomicU64>,
    events: broadcast::Sender<UpstreamMessage>,
    outbound: broadcast::Sender<Value>,
    state: Arc<Mutex<UpstreamConnectionState>>,
}

impl AppServerAdapter {
    pub fn new(epoch: u64) -> Self {
        let (events, _) = broadcast::channel(256);
        let (outbound, _) = broadcast::channel(256);
        Self {
            epoch: Arc::new(AtomicU64::new(epoch)),
            next_id: Arc::new(AtomicU64::new(1)),
            events,
            outbound,
            state: Arc::new(Mutex::new(UpstreamConnectionState::Stopped)),
        }
    }

    pub fn publish(&self, message: UpstreamMessage) {
        let _ = self.events.send(message);
    }

    pub fn subscribe_outbound(&self) -> broadcast::Receiver<Value> {
        self.outbound.subscribe()
    }
}

#[async_trait]
impl AppServerTransport for AppServerAdapter {
    async fn initialize(&self) -> Result<ServerIdentity, AppServerError> {
        *self.state.lock().await = UpstreamConnectionState::Initializing;
        let identity = ServerIdentity {
            name: "codex-app-server".into(),
            version: "0.146.1".into(),
            platform: Some("linux".into()),
            platform_family: Some("unix".into()),
            platform_os: Some("linux".into()),
            codex_home: None,
        };
        let _ = self.outbound.send(json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "vellum_remote_broker",
                    "title": "Vellum Remote Broker",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }
        }));
        let _ = self.outbound.send(json!({ "method": "initialized" }));
        *self.state.lock().await = UpstreamConnectionState::Ready;
        let _ = self.events.send(UpstreamMessage::Ready {
            epoch: self.epoch.load(Ordering::SeqCst),
            identity: identity.clone(),
        });
        Ok(identity)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, AppServerError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .outbound
            .send(json!({"id": id, "method": method, "params": params}));
        if method == "thread/start" {
            return Ok(json!({"thread": {"id": format!("thr_{id}")}}));
        }
        if method == "thread/resume" {
            let thread_id = params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Ok(json!({"thread": {"id": thread_id}}));
        }
        if method == "turn/start" {
            let turn_id = format!("turn_{id}");
            let thread_id = params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            self.publish(UpstreamMessage::Notification {
                epoch: self.epoch.load(Ordering::SeqCst),
                method: "turn/started".into(),
                params: json!({"threadId": thread_id, "turnId": turn_id}),
            });
            return Ok(json!({"turn": {"id": turn_id}}));
        }
        Ok(json!({"ok": true}))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), AppServerError> {
        let _ = self
            .outbound
            .send(json!({"method": method, "params": params}));
        Ok(())
    }

    async fn respond(&self, id: JsonRpcId, result: Value) -> Result<(), AppServerError> {
        let _ = self
            .outbound
            .send(json!({"id": id.to_value(), "result": result}));
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<UpstreamMessage> {
        self.events.subscribe()
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn connection_state(&self) -> UpstreamConnectionState {
        self.state
            .try_lock()
            .map(|g| *g)
            .unwrap_or(UpstreamConnectionState::Recovering)
    }
}
