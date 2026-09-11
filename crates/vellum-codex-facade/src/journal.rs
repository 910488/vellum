//! Event journal: `NOT_RUNTIME_REPLAY_SOURCE`.
//!
//! The journal exists for UI reconnect, audit, evaluation and debugging. It is
//! never an input to a native harness. If a native session is gone, the correct
//! behaviour is to ask that harness to resume or to surface the loss — never to
//! feed these entries back in and call it a resumed session.
//!
//! It stores *neutral* events rather than Codex notifications. Several native
//! events have no Codex representation at all (provider extensions, mid-flight
//! tool updates); keeping the neutral form means the audit and evaluation
//! record stays complete even where the UI shows nothing, and `thread/read`
//! derives its Codex view on demand.

use std::collections::HashMap;

use tokio::sync::Mutex;
use vellum_harness_protocol::HarnessEventEnvelope;

/// Compile-time marker mirroring the protocol constant, so a future change that
/// tries to make the journal authoritative has to delete this deliberately.
pub const NOT_RUNTIME_REPLAY_SOURCE: bool =
    !vellum_harness_protocol::EVENT_JOURNAL_RUNTIME_REPLAY_SOURCE;

const MAX_ENTRIES_PER_THREAD: usize = 4096;

/// One journalled event and the turn it belonged to, which the neutral
/// envelope does not carry on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub envelope: HarnessEventEnvelope,
    pub turn_id: Option<String>,
}

#[derive(Default)]
pub struct EventJournal {
    threads: Mutex<HashMap<String, Vec<JournalEntry>>>,
}

impl EventJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn append(&self, thread_id: &str, entry: JournalEntry) {
        let mut threads = self.threads.lock().await;
        let entries = threads.entry(thread_id.to_owned()).or_default();
        if entries.len() == MAX_ENTRIES_PER_THREAD {
            entries.remove(0);
        }
        entries.push(entry);
    }

    /// Read-back for UI reconnect, audit and evaluation only.
    pub async fn read(&self, thread_id: &str) -> Vec<JournalEntry> {
        self.threads
            .lock()
            .await
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn forget(&self, thread_id: &str) {
        self.threads.lock().await.remove(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use vellum_harness_protocol::{HarnessEvent, HarnessId, MessageDeltaEvent};

    fn entry(seq: u64) -> JournalEntry {
        JournalEntry {
            envelope: HarnessEventEnvelope {
                seq,
                harness_id: HarnessId("grok-build".into()),
                thread_id: "ui-a".into(),
                native_session_id: "native-1".into(),
                native_event_id: None,
                occurred_at: Utc::now(),
                event: HarnessEvent::AssistantDelta(MessageDeltaEvent {
                    message_id: None,
                    delta: seq.to_string(),
                }),
            },
            turn_id: Some("turn-1".into()),
        }
    }

    #[test]
    fn the_journal_is_declared_non_authoritative() {
        assert!(NOT_RUNTIME_REPLAY_SOURCE);
    }

    #[tokio::test]
    async fn threads_are_journalled_independently() {
        let journal = EventJournal::new();
        journal.append("ui-a", entry(1)).await;
        journal.append("ui-b", entry(2)).await;
        assert_eq!(journal.read("ui-a").await.len(), 1);
        assert_eq!(journal.read("ui-a").await[0].envelope.seq, 1);
        assert_eq!(journal.read("ui-b").await[0].envelope.seq, 2);
        journal.forget("ui-a").await;
        assert!(journal.read("ui-a").await.is_empty());
        assert_eq!(journal.read("ui-b").await.len(), 1);
    }

    #[tokio::test]
    async fn a_long_running_thread_does_not_grow_without_bound() {
        let journal = EventJournal::new();
        for seq in 0..(MAX_ENTRIES_PER_THREAD as u64 + 10) {
            journal.append("ui-a", entry(seq)).await;
        }
        let entries = journal.read("ui-a").await;
        assert_eq!(entries.len(), MAX_ENTRIES_PER_THREAD);
        assert_eq!(entries[0].envelope.seq, 10);
    }
}
