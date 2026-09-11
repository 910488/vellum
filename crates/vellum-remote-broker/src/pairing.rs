//! One-time pairing code support for local MVP.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{Duration, Utc};
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub code: String,
    pub expires_at_ms: i64,
}

#[derive(Default)]
pub struct PairingService {
    active: Mutex<HashMap<String, PairingCode>>,
}

impl PairingService {
    pub fn issue(&self) -> PairingCode {
        let code = Ulid::new().to_string()[..8].to_uppercase();
        let issued = PairingCode {
            code: code.clone(),
            expires_at_ms: (Utc::now() + Duration::minutes(10)).timestamp_millis(),
        };
        self.active
            .lock()
            .expect("pairing poisoned")
            .insert(code, issued.clone());
        issued
    }

    pub fn consume(&self, code: &str) -> bool {
        let mut guard = self.active.lock().expect("pairing poisoned");
        let now = Utc::now().timestamp_millis();
        matches!(guard.remove(code), Some(entry) if entry.expires_at_ms >= now)
    }
}
