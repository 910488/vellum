//! Downstream session registry.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::mpsc;
use vellum_remote_protocol::Envelope;

#[derive(Debug, Clone)]
pub struct ClientSession {
    pub device_id: String,
    pub connection_id: String,
}

#[derive(Clone)]
pub struct SessionRegistry {
    sessions: std::sync::Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Envelope<Value>>>>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            sessions: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SessionRegistry {
    pub fn insert(&self, connection_id: String, tx: mpsc::UnboundedSender<Envelope<Value>>) {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .insert(connection_id, tx);
    }

    pub fn remove(&self, connection_id: &str) {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .remove(connection_id);
    }

    pub fn broadcast(&self, message: Envelope<Value>) {
        let guard = self.sessions.lock().expect("session registry poisoned");
        for tx in guard.values() {
            let _ = tx.send(message.clone());
        }
    }

    pub fn connected_count(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .len()
    }
}
