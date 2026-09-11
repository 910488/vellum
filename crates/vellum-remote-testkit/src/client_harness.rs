//! Simple downstream client harness for reconnect tests.

use std::collections::HashMap;

use serde_json::Value;
use vellum_remote_protocol::{Envelope, ThreadEvent, ThreadSnapshot, PROTOCOL_VERSION};

#[derive(Debug, Default)]
pub struct ClientHarness {
    /// Highest contiguous event seq applied from the stream.
    pub last_applied_seq: u64,
    /// Cursor used for reconnect / resume (max of applied events and snapshot).
    pub resume_seq: u64,
    /// Last seq that should be ACKed on the wire (event stream only).
    pub last_event_ack_seq: u64,
    /// Backward-compatible alias for callers that still read last_ack_seq.
    pub last_ack_seq: u64,
    pub snapshot: Option<ThreadSnapshot>,
    pub events: Vec<ThreadEvent>,
    pub gap: bool,
    pub gap_count: u64,
    pub duplicate_count: u64,
}

impl ClientHarness {
    pub fn apply_snapshot(&mut self, snapshot: ThreadSnapshot) {
        // Snapshot may jump the resume cursor without being an event ACK.
        self.resume_seq = self.resume_seq.max(snapshot.snapshot_seq);
        self.last_applied_seq = self.last_applied_seq.max(snapshot.snapshot_seq);
        self.last_ack_seq = self.resume_seq;
        self.snapshot = Some(snapshot);
        // Snapshot itself is not a gap; keep prior gap stats.
    }

    /// Returns true when the event advances the contiguous cursor.
    pub fn apply_event(&mut self, event: ThreadEvent) -> bool {
        if event.seq <= self.last_applied_seq {
            self.duplicate_count = self.duplicate_count.saturating_add(1);
            return false;
        }
        if event.seq != self.last_applied_seq + 1 {
            self.gap = true;
            self.gap_count = self.gap_count.saturating_add(1);
            return false;
        }
        self.last_applied_seq = event.seq;
        self.last_event_ack_seq = event.seq;
        self.resume_seq = self.resume_seq.max(event.seq);
        self.last_ack_seq = self.resume_seq;
        self.events.push(event);
        true
    }

    pub fn hello_payload(&self, device_id: &str, thread_id: Option<&str>) -> Value {
        let mut payload = serde_json::json!({
            "deviceId": device_id,
            "clientVersion": "testkit",
            "capabilities": {
                "approvals": true,
                "artifacts": true,
                "writerLease": true
            }
        });
        if let Some(thread_id) = thread_id {
            payload["resume"] = serde_json::json!({
                "threadId": thread_id,
                "lastAckSeq": self.resume_seq
            });
        }
        payload
    }

    pub fn wrap(message_type: &str, payload: Value) -> Envelope<Value> {
        Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: ulid::Ulid::new().to_string(),
            r#type: message_type.to_string(),
            sent_at: chrono::Utc::now(),
            payload,
        }
    }

    pub fn event_index(&self) -> HashMap<u64, ThreadEvent> {
        self.events
            .iter()
            .cloned()
            .map(|event| (event.seq, event))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use vellum_remote_protocol::ThreadRuntimeStatus;

    fn event(seq: u64) -> ThreadEvent {
        ThreadEvent {
            seq,
            thread_id: "t1".into(),
            upstream_epoch: 1,
            method: "item/updated".into(),
            occurred_at: Utc::now(),
            turn_id: None,
            item_id: None,
            data: json!({}),
        }
    }

    #[test]
    fn counts_duplicates_and_gaps() {
        let mut h = ClientHarness::default();
        assert!(h.apply_event(event(1)));
        assert!(h.apply_event(event(2)));
        assert!(!h.apply_event(event(2)));
        assert_eq!(h.duplicate_count, 1);
        assert!(!h.apply_event(event(4)));
        assert_eq!(h.gap_count, 1);
        assert!(h.gap);
    }

    #[test]
    fn snapshot_advances_resume_cursor() {
        let mut h = ClientHarness::default();
        h.apply_snapshot(ThreadSnapshot {
            thread_id: "t1".into(),
            snapshot_seq: 10,
            status: ThreadRuntimeStatus::Completed,
            loaded: true,
            active_turn: None,
            pending_approvals: vec![],
            items: vec![],
            usage: None,
            writer_lease: None,
        });
        assert_eq!(h.resume_seq, 10);
        assert_eq!(h.last_applied_seq, 10);
        assert_eq!(h.last_event_ack_seq, 0);
    }
}
