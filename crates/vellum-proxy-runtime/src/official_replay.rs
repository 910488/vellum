//! Bounded, volatile Official wire history. Never persisted or used across realms.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::history::HistoryEntry;
use crate::request::request_input_items;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;
const TTL: Duration = Duration::from_secs(20 * 60);

struct Snapshot {
    entry: HistoryEntry,
    bytes: usize,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct OfficialReplayCache(Mutex<VecDeque<Snapshot>>);

impl OfficialReplayCache {
    pub(crate) fn get(&self, id: &str, route: &str, realm: Option<&str>) -> Option<HistoryEntry> {
        let realm = realm?;
        let mut entries = self.0.lock().ok()?;
        entries.retain(|snapshot| snapshot.created.elapsed() < TTL);
        entries
            .iter()
            .rev()
            .find(|snapshot| {
                snapshot.entry.response_id == id
                    && snapshot.entry.route_id == route
                    && snapshot.entry.continuation_realm.as_deref() == Some(realm)
            })
            .map(|snapshot| snapshot.entry.clone())
    }

    pub(crate) fn record(&self, body: &Value, response: &Value, route: &str, realm: Option<&str>) {
        let (Some(realm), Some(id)) = (realm, response.get("id").and_then(Value::as_str)) else {
            return;
        };
        let entry = HistoryEntry {
            response_id: id.to_owned(),
            previous_response_id: None,
            route_id: route.to_owned(),
            continuation_realm: Some(realm.to_owned()),
            input_items: request_input_items(body),
            output_items: response
                .get("output")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            created_at: crate::history::now_unix_secs(),
        };
        let Ok(encoded) = serde_json::to_vec(&entry) else {
            return;
        };
        let bytes = encoded.len();
        if bytes > MAX_BYTES {
            return;
        }
        let Ok(mut entries) = self.0.lock() else {
            return;
        };
        entries.retain(|snapshot| {
            snapshot.created.elapsed() < TTL
                && !(snapshot.entry.response_id == id
                    && snapshot.entry.continuation_realm.as_deref() == Some(realm))
        });
        let mut total = entries.iter().map(|snapshot| snapshot.bytes).sum::<usize>();
        while entries.len() >= MAX_ENTRIES || total + bytes > MAX_BYTES {
            let Some(oldest) = entries.pop_front() else {
                break;
            };
            total -= oldest.bytes;
        }
        entries.push_back(Snapshot {
            entry,
            bytes,
            created: Instant::now(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialized_payload_budget_evicts_old_snapshots() {
        let cache = OfficialReplayCache::default();
        let body = json!({"input":[{"role":"user","content":"x".repeat(1024 * 1024)}]});
        for n in 0..40 {
            cache.record(
                &body,
                &json!({"id":format!("resp_{n}"),"output":[]}),
                "route",
                Some("realm"),
            );
        }
        assert!(cache.get("resp_0", "route", Some("realm")).is_none());
        assert!(cache.get("resp_39", "route", Some("realm")).is_some());
        let entries = cache.0.lock().unwrap();
        assert!(entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES);
    }

    #[test]
    fn snapshots_are_realm_and_route_scoped_and_bounded() {
        let cache = OfficialReplayCache::default();
        for n in 0..=MAX_ENTRIES {
            cache.record(
                &json!({"input":[{"type":"reasoning","id":"rs_x","encrypted_content":"opaque"}]}),
                &json!({"id":format!("resp_{n}"),"output":[]}),
                "route",
                Some("account-a"),
            );
        }
        assert!(cache.get("resp_0", "route", Some("account-a")).is_none());
        assert!(cache.get("resp_256", "route", Some("account-a")).is_some());
        assert!(cache.get("resp_256", "route", Some("account-b")).is_none());
        assert!(cache
            .get("resp_256", "other-route", Some("account-a"))
            .is_none());
        assert!(cache.get("resp_256", "route", None).is_none());
        let mut entries = cache.0.lock().unwrap();
        for entry in entries.iter_mut() {
            entry.created = Instant::now() - TTL;
        }
        drop(entries);
        assert!(cache.get("resp_256", "route", Some("account-a")).is_none());
    }
}
