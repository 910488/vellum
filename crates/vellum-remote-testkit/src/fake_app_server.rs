//! Deterministic fake upstream app-server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

/// Minimal stand-in used by broker unit/integration tests.
#[derive(Clone)]
pub struct FakeAppServer {
    epoch: Arc<AtomicU64>,
    events: broadcast::Sender<Value>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Default for FakeAppServer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl FakeAppServer {
    pub fn new(epoch: u64) -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            epoch: Arc::new(AtomicU64::new(epoch)),
            events,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub async fn record_call(&self, method: &str, params: Value) {
        self.calls.lock().await.push((method.to_string(), params));
    }

    pub async fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().await.clone()
    }

    pub fn publish_notification(&self, method: &str, params: Value) {
        let _ = self.events.send(json!({
            "type": "notification",
            "method": method,
            "params": params
        }));
    }

    pub fn publish_server_request(&self, id: u64, method: &str, params: Value) {
        let _ = self.events.send(json!({
            "type": "request",
            "id": id,
            "method": method,
            "params": params
        }));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
}

#[async_trait]
pub trait FakeUpstream {
    async fn call(&self, method: &str, params: Value) -> Value;
}

#[async_trait]
impl FakeUpstream for FakeAppServer {
    async fn call(&self, method: &str, params: Value) -> Value {
        self.record_call(method, params.clone()).await;
        match method {
            "thread/start" => json!({"threadId": format!("fake_thread_{}", self.epoch())}),
            "turn/start" => json!({"turnId": format!("fake_turn_{}", self.epoch())}),
            _ => json!({"ok": true}),
        }
    }
}
