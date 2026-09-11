//! Durable continuation history (plan M4).
//!
//! Third-party endpoints return `store: false` and never remember the
//! previous turn; the proxy records every exchange itself and, when the next
//! request carries `previous_response_id`, replays the chain into `input`
//! before stripping that field (doc/05). This module owns that contract for
//! the shared runtime: the [`HistoryStore`] trait is the seam both Desktop's
//! Tauri-side SQLite store and the headless daemon's durable store implement,
//! with [`MemoryHistoryStore`] as the default (fixtures, tests) and
//! [`FileHistoryStore`] as the durable file-backed default until a real
//! daemon store is wired.
//!
//! Two invariants matter for parity:
//! - `record_exchange` persists the **normalized** response output (phase
//!   already assigned, provider ciphertext already stripped by
//!   `normalize_non_streaming_response`) together with the **original**
//!   request (which still carries `previous_response_id`, so chains link).
//! - `get_chain` walks `previous_response_id` links oldest → newest and must
//!   never loop (cycle guard, doc/05 trap).
//!
//! Token-budget truncation / emergency draining and compaction interplay are
//! deferred to M6; the fixtures use tiny chains, so the first M4 commit
//! hydrates the full chain (Desktop's `hydrate_input` with `usize::MAX`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::request::request_input_items;

/// One recorded exchange: the original request's input items plus the
/// normalized response's output items, linked into a chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub response_id: String,
    pub previous_response_id: Option<String>,
    /// The route that produced this exchange. Chain linking and the
    /// continuation decision (route switch ⇒ portable replay) read this.
    pub route_id: String,
    /// Stable, opaque provider/account realm that owns `response_id`.
    /// Official server-side response ids are valid only inside this realm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_realm: Option<String>,
    /// The original request's input items (doc/05 trap 1: never the
    /// translated/prepared upstream input).
    pub input_items: Vec<Value>,
    /// The normalized response's output items (phase assigned, ciphertext
    /// already stripped — nothing provider-private is ever persisted).
    pub output_items: Vec<Value>,
    pub created_at: i64,
}

/// Canonical compaction audit metadata persisted with a journal record (spec 53).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalAuditRecord {
    pub schema_version: u32,
    pub source_hash: String,
    pub checkpoint_hash: String,
    pub source_item_count: usize,
    pub source_tokens: u64,
    pub checkpoint_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_durable_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_compression_ratio: Option<f64>,
    pub semantic_claim_count: usize,
    pub grounded_claim_count: usize,
    pub rejected_claim_count: usize,
    pub prior_checkpoint_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_checkpoint_hash: Option<String>,
    pub repeated_sequences_detected: usize,
    pub repeated_exchanges_collapsed: usize,
    pub soft_trimmed_outputs: usize,
    pub hard_cleared_outputs: usize,
    pub extraction_attempts: u8,
    pub fallback_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<crate::diagnostics::CompactionFallbackReason>,
}

/// Durable compaction journal record (plan M6 / Canonical v2 spec 55).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionJournalRecord {
    pub response_id: String,
    pub compaction_id: String,
    pub generation: u32,
    /// Resolved engine that produced this record. Materialization accepts only
    /// the engine it is asked for, so a record written by a retired engine is a
    /// hard failure rather than a silent fallback.
    #[serde(default = "default_engine_id")]
    pub engine_id: String,
    #[serde(default)]
    pub engine_provenance: Option<String>,
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub tokens_before: u64,
    #[serde(default)]
    pub tokens_after: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default = "default_journal_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub source_items: Vec<Value>,
    pub canonical_items: Vec<Value>,
    #[serde(default)]
    pub source_hash: String,
    #[serde(default)]
    pub checkpoint_hash: String,
    /// The provider produced this compaction, not Vellum.
    ///
    /// OpenAI keeps the result as ciphertext Vellum never reads, so there is
    /// no "after" to journal. What `canonical_items` holds for such a row is
    /// the *pre*-compaction history, saved because that is what
    /// `materialize_local_compactions` expands the opaque marker back into
    /// when a third-party route later has to read the same conversation.
    ///
    /// The flag exists because the two shapes are indistinguishable in the
    /// stored columns, and the Context screen was reading the second one with
    /// the first one's rules -- reporting every Official compaction as
    /// "0 tokens before, the whole conversation after, engine: 舊版 Vellum
    /// 壓縮紀錄", which is the compaction backwards.
    #[serde(default)]
    pub provider_owned: bool,
}

fn default_journal_schema_version() -> u32 {
    1
}

/// A record with no engine id predates the switchover, so it can only have come
/// from the retired Canonical engine.
fn default_engine_id() -> String {
    "vellum_canonical_retired".to_string()
}

/// Schema version for [`LocalCompactionRecordV1`].
pub const LOCAL_COMPACTION_RECORD_SCHEMA_V1: u32 = 1;

/// Engine-neutral durable record of one locally materialized compaction.
///
/// This replaces the Canonical-shaped journal: it stores no checkpoint schema,
/// no semantic delta, and nothing an engine has to parse back out. It records
/// what was compacted, what replaced it, which engine and upstream identity
/// produced it, and — when the attempt failed — why.
///
/// `source_items` are the **sanitized** items that were sent to the
/// summarizer, so replaying a record never re-exposes provider-private
/// material that sanitization removed. `source_hash` and `replacement_hash`
/// are over those exact item vectors, which lets materialization fail closed
/// on a corrupt or tampered record instead of silently resuming from the
/// wrong history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalCompactionRecordV1 {
    #[serde(default = "default_local_compaction_schema_version")]
    pub schema_version: u32,
    /// Upstream response id this compaction is bound to.
    pub response_id: String,
    /// Vellum-minted local compaction id, carrying the engine marker prefix.
    pub compaction_id: String,
    /// Monotonic generation within a conversation's compaction lineage.
    pub generation: u32,
    /// Resolved engine id, e.g. `codex_local_v0_150`. Materialization accepts
    /// only the engine it is asked for; a record from another engine is a
    /// hard failure, never a silent fallback.
    pub engine_id: String,
    /// Upstream provenance string for the engine, where one exists.
    #[serde(default)]
    pub engine_provenance: Option<String>,
    /// Sanitized pre-compaction history that was summarized.
    #[serde(default)]
    pub source_items: Vec<Value>,
    /// SHA-256 over `source_items`.
    #[serde(default)]
    pub source_hash: String,
    /// Items installed in place of `source_items`.
    #[serde(default)]
    pub replacement_items: Vec<Value>,
    /// SHA-256 over `replacement_items`.
    #[serde(default)]
    pub replacement_hash: String,
    /// Route that performed the compaction.
    #[serde(default)]
    pub route_id: String,
    /// Upstream model that produced the summary — always the session's own
    /// model, never a separate compactor.
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub tokens_before: u64,
    #[serde(default)]
    pub tokens_after: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// Stable failure code when the attempt did not produce a replacement.
    /// `None` means the compaction succeeded.
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

fn default_local_compaction_schema_version() -> u32 {
    LOCAL_COMPACTION_RECORD_SCHEMA_V1
}

impl LocalCompactionRecordV1 {
    /// Verify the record's own integrity before it is used to rebuild history.
    ///
    /// A record whose hashes do not match its items cannot be trusted to
    /// describe what actually happened, so materialization must fail closed
    /// rather than install the items anyway.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != LOCAL_COMPACTION_RECORD_SCHEMA_V1 {
            return Err(format!(
                "unsupported local compaction record schema {}",
                self.schema_version
            ));
        }
        if self.engine_id.is_empty() {
            return Err("local compaction record has no engine id".into());
        }
        let source = hash_items(&self.source_items);
        if source != self.source_hash {
            return Err("local compaction record source hash mismatch".into());
        }
        let replacement = hash_items(&self.replacement_items);
        if replacement != self.replacement_hash {
            return Err("local compaction record replacement hash mismatch".into());
        }
        Ok(())
    }

    /// Convert the engine-neutral record into the legacy storage envelope.
    ///
    /// The physical journal remains readable during the transition, but new
    /// production code constructs this type first. This keeps Canonical field
    /// names out of the engine contract while preserving existing encrypted
    /// history rows and response bindings.
    fn into_storage_record(self) -> CompactionJournalRecord {
        CompactionJournalRecord {
            response_id: self.response_id,
            compaction_id: self.compaction_id,
            generation: self.generation,
            engine_id: self.engine_id,
            engine_provenance: self.engine_provenance,
            route_id: self.route_id,
            upstream_model: self.upstream_model,
            tokens_before: self.tokens_before,
            tokens_after: self.tokens_after,
            elapsed_ms: self.elapsed_ms,
            schema_version: self.schema_version,
            source_items: self.source_items,
            canonical_items: self.replacement_items,
            source_hash: self.source_hash,
            checkpoint_hash: self.replacement_hash,
            // A `LocalCompactionRecordV1` is by definition one this runtime
            // produced. The provider-owned shape is written straight to the
            // storage record instead, because it has no replacement half.
            provider_owned: false,
        }
    }
}

/// SHA-256 over a canonical JSON encoding of an item vector.
pub fn hash_items(items: &[Value]) -> String {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_string(items).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(encoded.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Structured compacted replay result carrying exact canonical prefix provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactedReplayProvenance {
    pub items: Vec<Value>,
    pub canonical_prefix_len: usize,
    pub parent_compaction_id: Option<String>,
}

/// A durable compaction journal (plan M6 / Canonical v2): binds a local
/// canonical checkpoint to the response it replaced, preserving both the
/// installed canonical items and the sanitized source items so later
/// compacts (or audits) can chain reliably.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionJournal {
    /// The `cmp_vellum_<hex>` checkpoint id.
    pub compaction_id: String,
    /// Resolved engine that produced this record.
    #[serde(default = "default_engine_id")]
    pub engine_id: String,
    #[serde(default)]
    pub engine_provenance: Option<String>,
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub tokens_before: u64,
    #[serde(default)]
    pub tokens_after: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// The response being compacted (the compact request's
    /// `previous_response_id`).
    pub response_id: String,
    /// 1-based generation; each compact for the same head increments it.
    pub generation: u32,
    /// The installed canonical window items (summary message + retained
    /// tail) that replace the compacted source.
    pub canonical_items: Vec<Value>,
    /// Sanitized source items compacted to produce this window (spec 54–57).
    #[serde(default)]
    pub source_items: Vec<Value>,
    /// Schema version of the canonical checkpoint (1 for v1, 2 for v2).
    #[serde(default = "default_journal_schema_version")]
    pub schema_version: u32,
    /// Hash of sanitized source history.
    #[serde(default)]
    pub source_hash: String,
    /// Hash of installed checkpoint items.
    #[serde(default)]
    pub checkpoint_hash: String,
    /// Local audit metrics (if present).
    pub created_at: i64,
}

impl CompactionJournal {
    pub fn from_record(record: CompactionJournalRecord) -> Self {
        Self {
            compaction_id: record.compaction_id,
            engine_id: record.engine_id,
            engine_provenance: record.engine_provenance,
            route_id: record.route_id,
            upstream_model: record.upstream_model,
            tokens_before: record.tokens_before,
            tokens_after: record.tokens_after,
            elapsed_ms: record.elapsed_ms,
            response_id: record.response_id,
            generation: record.generation,
            canonical_items: record.canonical_items,
            source_items: record.source_items,
            schema_version: record.schema_version,
            source_hash: record.source_hash,
            checkpoint_hash: record.checkpoint_hash,
            created_at: now_unix_secs(),
        }
    }
}

/// Durable history backing. Implementations must be `Send + Sync` (the
/// runtime executes requests concurrently across tasks).
pub trait HistoryStore: Send + Sync {
    /// Persist the engine-neutral local-compaction contract used by all new
    /// compaction engines. Implementations may keep their existing physical
    /// journal envelope while old encrypted rows age out.
    fn record_local_compaction(&self, record: LocalCompactionRecordV1) -> Result<(), String> {
        record.validate()?;
        self.record_compaction_record(record.into_storage_record())
    }

    /// Record a compaction journal binding `compaction_id` (with its
    /// installed canonical window) to `response_id`. Stores that do not
    /// journal yet accept the record silently.
    fn record_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
        generation: u32,
        canonical_items: Vec<Value>,
    ) -> Result<(), String> {
        let _ = (response_id, compaction_id, generation, canonical_items);
        Ok(())
    }

    /// Record a full compaction journal record including sanitized source
    /// items, schema version, hashes, and audit record (spec 54).
    fn record_compaction_record(&self, record: CompactionJournalRecord) -> Result<(), String> {
        self.record_compaction(
            &record.response_id,
            &record.compaction_id,
            record.generation,
            record.canonical_items,
        )
    }

    /// Retrieve the journal record for a response_id, if available.
    /// Load the durable recovery snapshot for a conversation.
    ///
    /// Default is "no snapshot", which is correct for a store that does not
    /// persist recovery: the conversation simply starts without prior
    /// recovery state rather than failing.
    fn recovery_snapshot(
        &self,
        _conversation_key: &str,
    ) -> Result<Option<crate::recovery_snapshot::ConversationRecoverySnapshotV1>, String> {
        Ok(None)
    }

    /// Persist the durable recovery snapshot for a conversation.
    fn put_recovery_snapshot(
        &self,
        _snapshot: &crate::recovery_snapshot::ConversationRecoverySnapshotV1,
    ) -> Result<(), String> {
        Ok(())
    }

    fn journal_record(&self, response_id: &str) -> Result<Option<CompactionJournal>, String> {
        let _ = response_id;
        Ok(None)
    }

    /// Retrieve the journal record for a compaction_id, if available.
    fn journal_record_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<CompactionJournal>, String> {
        let _ = compaction_id;
        Ok(None)
    }

    /// All compaction ids journaled for `response_id`, oldest first.
    fn compaction_ids_for_response(&self, response_id: &str) -> Result<Vec<String>, String> {
        let _ = response_id;
        Ok(Vec::new())
    }

    /// The generation of the newest journal for `compaction_id`, if any.
    fn compaction_generation(&self, compaction_id: &str) -> Result<Option<u32>, String> {
        let _ = compaction_id;
        Ok(None)
    }

    /// The installed canonical window items journaled *directly* for
    /// `response_id`, when a journal exists.
    fn journal_canonical_items(&self, response_id: &str) -> Result<Option<Vec<Value>>, String> {
        let _ = response_id;
        Ok(None)
    }

    /// Resolve a Vellum-owned opaque compaction reference back to its
    /// canonical window. This is intentionally keyed by compaction id rather
    /// than response id because Codex replays the opaque item on the next
    /// stateless turn.
    fn canonical_items_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<Vec<Value>>, String> {
        let _ = compaction_id;
        Ok(None)
    }

    /// The compacted replay with structured provenance for `head`: the newest journal
    /// found while walking the response chain, plus every exchange recorded after that
    /// compaction. Carries the exact canonical prefix length and parent compaction ID
    /// so callers can assign precise replay identities without guessing.
    fn compacted_replay_with_provenance(
        &self,
        head: &str,
    ) -> Result<Option<CompactedReplayProvenance>, String> {
        let chain = self.get_chain(head)?;
        let mut after: Vec<Value> = Vec::new();
        for entry in chain.iter().rev() {
            if let Some(items) = self.journal_canonical_items(&entry.response_id)? {
                let canonical_prefix_len = items.len();
                let parent_compaction_id = self
                    .compaction_ids_for_response(&entry.response_id)?
                    .into_iter()
                    .last();
                let mut out = items;
                out.extend(after);
                return Ok(Some(CompactedReplayProvenance {
                    items: out,
                    canonical_prefix_len,
                    parent_compaction_id,
                }));
            }
            let mut exchange_items = entry.input_items.clone();
            exchange_items.extend(entry.output_items.clone());
            after.splice(0..0, exchange_items);
        }

        // If the chain stops at an exchange whose previous_response_id is a journaled
        // compaction (or if head itself was a compaction id), look up the journal on that id.
        let root_id = chain
            .first()
            .and_then(|e| e.previous_response_id.as_deref())
            .unwrap_or(head);

        // 1. Check if root_id is directly a compaction id (e.g. cmp_xxx)
        if let Some(journal) = self.journal_record_for_compaction(root_id)? {
            let canonical_prefix_len = journal.canonical_items.len();
            let mut out = journal.canonical_items;
            out.extend(after);
            return Ok(Some(CompactedReplayProvenance {
                items: out,
                canonical_prefix_len,
                parent_compaction_id: Some(journal.compaction_id),
            }));
        }

        // 2. Check if root_id is a response_id that owns a journal
        if let Some(items) = self.journal_canonical_items(root_id)? {
            let canonical_prefix_len = items.len();
            let parent_compaction_id = self
                .compaction_ids_for_response(root_id)?
                .into_iter()
                .last();
            let mut out = items;
            out.extend(after);
            return Ok(Some(CompactedReplayProvenance {
                items: out,
                canonical_prefix_len,
                parent_compaction_id,
            }));
        }

        Ok(None)
    }

    /// The compacted replay for `head`: the newest journal found while
    /// walking the response chain, plus every exchange recorded after that
    /// compaction (Desktop `compacted_replay_chain` equivalent used to
    /// rewrite a follow-up compact's input).
    fn compacted_replay_items(&self, head: &str) -> Result<Option<Vec<Value>>, String> {
        Ok(self
            .compacted_replay_with_provenance(head)?
            .map(|r| r.items))
    }

    /// The compaction id of the newest journal found while walking the
    /// response chain from `head` (the parent for the next compaction).
    fn compacted_replay_parent(&self, head: &str) -> Result<Option<String>, String> {
        let chain = self.get_chain(head)?;
        for entry in chain.iter().rev() {
            let ids = self.compaction_ids_for_response(&entry.response_id)?;
            if let Some(id) = ids.last() {
                return Ok(Some(id.clone()));
            }
        }
        let root_id = chain
            .first()
            .and_then(|e| e.previous_response_id.as_deref())
            .unwrap_or(head);

        if self.journal_record_for_compaction(root_id)?.is_some() {
            return Ok(Some(root_id.to_string()));
        }

        let ids = self.compaction_ids_for_response(root_id)?;
        Ok(ids.into_iter().last())
    }

    /// Persist one exchange. `request` is the original client request (with
    /// `previous_response_id` preserved for chain linking) and `response` is
    /// the normalized output. Returns `Ok(false)` when the response carries
    /// no usable `id` — there is nothing to chain from, so nothing is
    /// recorded (mirrors Desktop).
    fn record_exchange(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
    ) -> Result<bool, String>;

    /// Walk the `previous_response_id` chain starting at `head`, oldest →
    /// newest. Unknown ids stop the walk (the caller decides how to fail).
    fn get_chain(&self, head: &str) -> Result<Vec<HistoryEntry>, String>;

    /// Like [`Self::record_exchange`], but an explicit conversation key
    /// (resolved session identity, e.g. from [`crate::grok_session`])
    /// overrides re-derivation from the request body, so a session whose
    /// identity does not live in one of the request body's own fields (a
    /// caller-supplied session hint) still records under one durable
    /// conversation for restart recovery. Stores that do not track
    /// conversation identity fall back to [`Self::record_exchange`] and
    /// silently ignore the override.
    fn record_exchange_with_conversation_key(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
    ) -> Result<bool, String> {
        let _ = conversation_key;
        self.record_exchange(request, response, route_id)
    }

    /// Like [`Self::record_exchange_with_conversation_key`], with the
    /// account-scoped continuation realm resolved for this completed turn.
    /// Older store implementations may ignore the realm, but production
    /// stores override this so a later account switch can reject native
    /// `previous_response_id` reuse before it reaches the provider.
    fn record_exchange_with_context(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
        continuation_realm: Option<&str>,
    ) -> Result<bool, String> {
        let _ = continuation_realm;
        self.record_exchange_with_conversation_key(request, response, route_id, conversation_key)
    }

    /// Realm that owns a durable response id. A missing value denotes a
    /// legacy row; callers may use live connection evidence but must never
    /// invent an account identity for it.
    fn response_continuation_realm(&self, response_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .get_chain(response_id)?
            .last()
            .and_then(|entry| entry.continuation_realm.clone()))
    }

    /// Durable recovery of conversation identity for a response id (restart
    /// / in-memory registry miss / LRU eviction). Stores that do not track
    /// conversation identity report `Ok(None)` — a miss, never an error, so
    /// callers fail closed only on a genuine lookup failure.
    fn response_conversation_key(&self, response_id: &str) -> Result<Option<String>, String> {
        let _ = response_id;
        Ok(None)
    }

    /// Number of completed exchanges recorded for one route within one
    /// conversation. Used to seed a turn counter after a restart so it picks
    /// up where the durable history left off instead of forking from zero.
    fn conversation_route_exchange_count(
        &self,
        conversation_key: &str,
        route_id: &str,
    ) -> Result<u64, String> {
        let _ = (conversation_key, route_id);
        Ok(0)
    }

    /// Latest durable response chain for a conversation, optionally restricted
    /// to one route. Memory and File stores must agree. An empty vec is a
    /// miss. Multiple leaf heads are an error, never a HashMap-order pick.
    fn latest_chain_for_conversation(
        &self,
        conversation_key: &str,
        route_id: Option<&str>,
    ) -> Result<Vec<HistoryEntry>, String> {
        let _ = (conversation_key, route_id);
        Ok(Vec::new())
    }

    /// Response id at the head of [`Self::latest_chain_for_conversation`].
    fn latest_response_id_for_conversation(
        &self,
        conversation_key: &str,
        route_id: Option<&str>,
    ) -> Result<Option<String>, String> {
        Ok(self
            .latest_chain_for_conversation(conversation_key, route_id)?
            .last()
            .map(|entry| entry.response_id.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationLeafLookup {
    Missing,
    Unique(String),
    Ambiguous(Vec<String>),
}

fn conversation_leaf_lookup(
    entries: &HashMap<String, HistoryEntry>,
    keys: &HashMap<String, String>,
    conversation_key: &str,
    route_id: Option<&str>,
) -> ConversationLeafLookup {
    let matching: Vec<&HistoryEntry> = entries
        .values()
        .filter(|entry| {
            keys.get(&entry.response_id).map(String::as_str) == Some(conversation_key)
                && route_id.is_none_or(|route| entry.route_id == route)
        })
        .collect();
    if matching.is_empty() {
        return ConversationLeafLookup::Missing;
    }
    let referenced: HashSet<&str> = matching
        .iter()
        .filter_map(|entry| entry.previous_response_id.as_deref())
        .collect();
    let mut leaves: Vec<String> = matching
        .into_iter()
        .filter(|entry| !referenced.contains(entry.response_id.as_str()))
        .map(|entry| entry.response_id.clone())
        .collect();
    leaves.sort();
    match leaves.as_slice() {
        [] => ConversationLeafLookup::Missing,
        [head] => ConversationLeafLookup::Unique(head.clone()),
        _ => ConversationLeafLookup::Ambiguous(leaves),
    }
}

fn chain_for_conversation_leaf(
    entries: &HashMap<String, HistoryEntry>,
    keys: &HashMap<String, String>,
    conversation_key: &str,
    route_id: Option<&str>,
) -> Result<Vec<HistoryEntry>, String> {
    match conversation_leaf_lookup(entries, keys, conversation_key, route_id) {
        ConversationLeafLookup::Missing => Ok(Vec::new()),
        ConversationLeafLookup::Unique(head) => Ok(walk_chain(entries, &head)),
        ConversationLeafLookup::Ambiguous(leaves) => Err(format!(
            "ambiguous conversation branch: multiple leaf heads {}",
            leaves.join(",")
        )),
    }
}

/// Build a conversation key scoped to a durable Codex session and thread identity.
pub fn codex_conversation_key(
    identity: &crate::codex_metadata::CodexTurnIdentity,
) -> Option<String> {
    if identity.trust == crate::codex_metadata::CodexIdentityTrust::Conflict {
        return None;
    }
    let session_id = identity.session_id.as_ref()?;
    let thread_id = identity.thread_id.as_ref()?;
    Some(format!(
        "codex:{}:{}",
        encode_codex_conversation_key_component(session_id.as_str()),
        encode_codex_conversation_key_component(thread_id.as_str())
    ))
}

/// Encode the two opaque Codex identity components without changing the
/// durable key generated for the UUID-like identifiers used by current
/// clients. `:` is the component delimiter and `%` is the escape marker, so
/// both must be encoded to keep future opaque identifiers collision-free.
fn encode_codex_conversation_key_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            ':' => encoded.push_str("%3A"),
            _ => encoded.push(character),
        }
    }
    encoded
}

/// Source of a resolved conversation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKeySource {
    CodexThreadIdentity,
    ExistingConversationKey,
    PreviousResponseChain,
    GeneratedFallback,
}

/// Outcome of resolving a conversation key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationKeyResolution {
    pub key: String,
    pub source: ConversationKeySource,
}

/// Resolve the conversation key for a runtime request.
pub fn resolve_conversation_key(
    request: &crate::request::RuntimeRequest,
    existing_fallback: Option<&str>,
) -> ConversationKeyResolution {
    if let Some(identity) = &request.metadata.codex_identity {
        if let Some(key) = codex_conversation_key(identity) {
            return ConversationKeyResolution {
                key,
                source: ConversationKeySource::CodexThreadIdentity,
            };
        }
    }

    if let Some(fallback) = existing_fallback.filter(|f| !f.trim().is_empty()) {
        return ConversationKeyResolution {
            key: fallback.to_string(),
            source: ConversationKeySource::ExistingConversationKey,
        };
    }

    if let Some(grok_key) = crate::grok_session::conversation_key_from_request(&request.body) {
        return ConversationKeyResolution {
            key: grok_key,
            source: ConversationKeySource::ExistingConversationKey,
        };
    }

    if let Some(prev_id) = request
        .body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
    {
        return ConversationKeyResolution {
            key: format!("chain:{prev_id}"),
            source: ConversationKeySource::PreviousResponseChain,
        };
    }

    let generated = format!("gen_{}", request.metadata.request_id);
    ConversationKeyResolution {
        key: generated,
        source: ConversationKeySource::GeneratedFallback,
    }
}

fn inherit_conversation_key(
    request: &Value,
    previous_response_id: Option<&str>,
    existing: &HashMap<String, String>,
) -> Option<String> {
    if let Ok(Some(identity)) = crate::codex_metadata::parse_canonical_client_metadata(request) {
        if let Some(key) = codex_conversation_key(&identity) {
            return Some(key);
        }
    }
    if let Ok(compat) = crate::codex_metadata::parse_compatibility_projection(
        &axum::http::HeaderMap::new(),
        request,
    ) {
        if let (Some(s), Some(t)) = (&compat.session_id, &compat.thread_id) {
            let identity = crate::codex_metadata::CodexTurnIdentity {
                session_id: Some(s.clone()),
                thread_id: Some(t.clone()),
                source: crate::codex_metadata::CodexIdentitySource::FlatCompatibilityHeaders,
                trust: crate::codex_metadata::CodexIdentityTrust::Partial,
                ..Default::default()
            };
            return codex_conversation_key(&identity);
        }
    }
    crate::grok_session::conversation_key_from_request(request)
        .or_else(|| previous_response_id.and_then(|id| existing.get(id).cloned()))
}

pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn entry_from_exchange(
    request: &Value,
    response: &Value,
    route_id: &str,
    continuation_realm: Option<&str>,
    created_at: i64,
) -> Option<HistoryEntry> {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)?;
    let previous_response_id = request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string);
    let input_items = request_input_items(request);
    let output_items = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some(HistoryEntry {
        response_id,
        previous_response_id,
        route_id: route_id.to_string(),
        continuation_realm: continuation_realm.map(str::to_string),
        input_items,
        output_items,
        created_at,
    })
}

/// In-memory history store — the default for fixtures and tests, and the
/// simplest correct `HistoryStore` implementation.
#[derive(Debug, Default)]
pub struct MemoryHistoryStore {
    entries: Mutex<HashMap<String, HistoryEntry>>,
    journals: Mutex<HashMap<String, Vec<CompactionJournal>>>,
    /// `response_id -> conversation_key`, set only via
    /// [`HistoryStore::record_exchange_with_conversation_key`].
    conversation_keys: Mutex<HashMap<String, String>>,
    /// Durable recovery state keyed by conversation, so it follows the
    /// conversation across a provider switch rather than a route.
    recovery_snapshots:
        Mutex<HashMap<String, crate::recovery_snapshot::ConversationRecoverySnapshotV1>>,
}

impl MemoryHistoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl HistoryStore for MemoryHistoryStore {
    fn recovery_snapshot(
        &self,
        conversation_key: &str,
    ) -> Result<Option<crate::recovery_snapshot::ConversationRecoverySnapshotV1>, String> {
        Ok(self
            .recovery_snapshots
            .lock()
            .map_err(|_| "recovery snapshot map poisoned".to_string())?
            .get(conversation_key)
            .cloned())
    }

    fn put_recovery_snapshot(
        &self,
        snapshot: &crate::recovery_snapshot::ConversationRecoverySnapshotV1,
    ) -> Result<(), String> {
        snapshot.validate()?;
        self.recovery_snapshots
            .lock()
            .map_err(|_| "recovery snapshot map poisoned".to_string())?
            .insert(snapshot.conversation_key.clone(), snapshot.clone());
        Ok(())
    }

    fn record_exchange(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
    ) -> Result<bool, String> {
        self.record_exchange_with_conversation_key(request, response, route_id, None)
    }

    fn get_chain(&self, head: &str) -> Result<Vec<HistoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(walk_chain(&entries, head))
    }

    fn record_exchange_with_conversation_key(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
    ) -> Result<bool, String> {
        self.record_exchange_with_context(request, response, route_id, conversation_key, None)
    }

    fn record_exchange_with_context(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
        continuation_realm: Option<&str>,
    ) -> Result<bool, String> {
        let Some(entry) = entry_from_exchange(
            request,
            response,
            route_id,
            continuation_realm,
            now_unix_secs(),
        ) else {
            return Ok(false);
        };
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        let mut keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        let inherited =
            inherit_conversation_key(request, entry.previous_response_id.as_deref(), &keys);
        let key = conversation_key
            .filter(|key| !key.trim().is_empty())
            .map(str::to_string)
            .or(inherited);
        entries.insert(entry.response_id.clone(), entry.clone());
        if let Some(key) = key {
            keys.insert(entry.response_id.clone(), key);
        }
        Ok(true)
    }

    fn latest_chain_for_conversation(
        &self,
        conversation_key: &str,
        route_id: Option<&str>,
    ) -> Result<Vec<HistoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        let keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        chain_for_conversation_leaf(&entries, &keys, conversation_key, route_id)
    }

    fn response_conversation_key(&self, response_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .conversation_keys
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?
            .get(response_id)
            .cloned())
    }

    fn conversation_route_exchange_count(
        &self,
        conversation_key: &str,
        route_id: &str,
    ) -> Result<u64, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        let keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(entries
            .values()
            .filter(|entry| {
                entry.route_id == route_id
                    && keys.get(&entry.response_id).map(String::as_str) == Some(conversation_key)
            })
            .count() as u64)
    }

    fn record_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
        generation: u32,
        canonical_items: Vec<Value>,
    ) -> Result<(), String> {
        let source_items = canonical_items.clone();
        let source_hash = hash_items(&source_items);
        let checkpoint_hash = hash_items(&canonical_items);
        self.record_compaction_record(CompactionJournalRecord {
            response_id: response_id.to_string(),
            compaction_id: compaction_id.to_string(),
            generation,
            engine_id: crate::codex_local_v0_150::ENGINE_ID.to_string(),
            engine_provenance: Some(crate::codex_local_v0_150::ENGINE_PROVENANCE.to_string()),
            route_id: String::new(),
            upstream_model: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            elapsed_ms: 0,
            schema_version: LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            source_items,
            canonical_items,
            source_hash,
            checkpoint_hash,
            provider_owned: false,
        })
    }

    fn record_compaction_record(&self, record: CompactionJournalRecord) -> Result<(), String> {
        let response_id = record.response_id.clone();
        let journal = CompactionJournal::from_record(record);
        self.journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?
            .entry(response_id)
            .or_default()
            .push(journal);
        Ok(())
    }

    fn journal_record(&self, response_id: &str) -> Result<Option<CompactionJournal>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .and_then(|list| list.last().cloned()))
    }

    fn journal_record_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<CompactionJournal>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .cloned())
    }

    fn compaction_ids_for_response(&self, response_id: &str) -> Result<Vec<String>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .map(|list| {
                list.iter()
                    .map(|journal| journal.compaction_id.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn compaction_generation(&self, compaction_id: &str) -> Result<Option<u32>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .map(|journal| journal.generation))
    }

    fn journal_canonical_items(&self, response_id: &str) -> Result<Option<Vec<Value>>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .and_then(|list| list.last())
            .map(|journal| journal.canonical_items.clone()))
    }

    fn canonical_items_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<Vec<Value>>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "memory history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .map(|journal| journal.canonical_items.clone()))
    }
}

fn walk_chain(entries: &HashMap<String, HistoryEntry>, head: &str) -> Vec<HistoryEntry> {
    let mut newest_first = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(head.to_string());
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            break; // cycle guard
        }
        let Some(entry) = entries.get(&id) else {
            break; // unknown id stops the walk
        };
        current = entry.previous_response_id.clone();
        newest_first.push(entry.clone());
    }
    newest_first.reverse();
    newest_first
}

/// Durable file-backed history store: JSON-lines under one path, rewritten
/// atomically (temp + rename) on every record. Small-scale by design — it
/// exists so restart continuation is real and testable until the daemon
/// wires its production store.
#[derive(Debug)]
pub struct FileHistoryStore {
    path: PathBuf,
    entries: Mutex<HashMap<String, HistoryEntry>>,
    journals: Mutex<HashMap<String, Vec<CompactionJournal>>>,
    /// `response_id -> conversation_key`, persisted alongside entries/
    /// journals so restart recovery (M4's own guarantee) extends to session
    /// identity lookups, not just the response chain.
    conversation_keys: Mutex<HashMap<String, String>>,
}

impl FileHistoryStore {
    /// Open (creating if missing) a durable store at `path`. Existing
    /// records are loaded so a restarted runtime picks the chain back up.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut entries = HashMap::new();
        let mut journals: HashMap<String, Vec<CompactionJournal>> = HashMap::new();
        let mut conversation_keys: HashMap<String, String> = HashMap::new();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|error| format!("read history file {}: {error}", path.display()))?;
            for (line_index, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                    entries.insert(entry.response_id.clone(), entry);
                    continue;
                }
                // Journal lines are wrapped with a `kind` marker.
                let wrapped: serde_json::Value = serde_json::from_str(line).map_err(|error| {
                    format!(
                        "parse history line {} in {}: {error}",
                        line_index + 1,
                        path.display()
                    )
                })?;
                if wrapped.get("kind").and_then(Value::as_str) == Some("compaction") {
                    let journal: CompactionJournal =
                        serde_json::from_value(wrapped.get("journal").cloned().unwrap_or_default())
                            .map_err(|error| {
                                format!(
                                    "parse compaction journal line {} in {}: {error}",
                                    line_index + 1,
                                    path.display()
                                )
                            })?;
                    journals
                        .entry(journal.response_id.clone())
                        .or_default()
                        .push(journal);
                    continue;
                }
                if wrapped.get("kind").and_then(Value::as_str) == Some("conversation_key") {
                    let response_id = wrapped
                        .get("response_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let key = wrapped
                        .get("conversation_key")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !response_id.is_empty() && !key.is_empty() {
                        conversation_keys.insert(response_id.to_string(), key.to_string());
                    }
                    continue;
                }
                return Err(format!(
                    "unrecognized history line {} in {}",
                    line_index + 1,
                    path.display()
                ));
            }
        }
        Ok(Self {
            path,
            entries: Mutex::new(entries),
            journals: Mutex::new(journals),
            conversation_keys: Mutex::new(conversation_keys),
        })
    }

    fn persist_locked(
        &self,
        entries: &HashMap<String, HistoryEntry>,
        journals: &HashMap<String, Vec<CompactionJournal>>,
        conversation_keys: &HashMap<String, String>,
    ) -> Result<(), String> {
        let mut contents = String::new();
        for entry in entries.values() {
            let line = serde_json::to_string(entry)
                .map_err(|error| format!("serialize history entry: {error}"))?;
            contents.push_str(&line);
            contents.push('\n');
        }
        for journal in journals.values().flatten() {
            let wrapped = serde_json::json!({
                "kind": "compaction",
                "journal": journal
            });
            let line = serde_json::to_string(&wrapped)
                .map_err(|error| format!("serialize compaction journal: {error}"))?;
            contents.push_str(&line);
            contents.push('\n');
        }
        for (response_id, key) in conversation_keys {
            let wrapped = serde_json::json!({
                "kind": "conversation_key",
                "response_id": response_id,
                "conversation_key": key
            });
            let line = serde_json::to_string(&wrapped)
                .map_err(|error| format!("serialize conversation key: {error}"))?;
            contents.push_str(&line);
            contents.push('\n');
        }
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, contents)
            .map_err(|error| format!("write history temp {}: {error}", temp.display()))?;
        std::fs::rename(&temp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!("replace history file {}: {error}", self.path.display())
        })
    }
}

impl HistoryStore for FileHistoryStore {
    fn record_exchange(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
    ) -> Result<bool, String> {
        self.record_exchange_with_conversation_key(request, response, route_id, None)
    }

    fn record_exchange_with_conversation_key(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
    ) -> Result<bool, String> {
        self.record_exchange_with_context(request, response, route_id, conversation_key, None)
    }

    fn record_exchange_with_context(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
        continuation_realm: Option<&str>,
    ) -> Result<bool, String> {
        let Some(entry) = entry_from_exchange(
            request,
            response,
            route_id,
            continuation_realm,
            now_unix_secs(),
        ) else {
            return Ok(false);
        };
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let mut conversation_keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let inherited = inherit_conversation_key(
            request,
            entry.previous_response_id.as_deref(),
            &conversation_keys,
        );
        let key = conversation_key
            .filter(|key| !key.trim().is_empty())
            .map(str::to_string)
            .or(inherited);
        entries.insert(entry.response_id.clone(), entry.clone());
        if let Some(key) = key {
            conversation_keys.insert(entry.response_id.clone(), key);
        }
        self.persist_locked(&entries, &journals, &conversation_keys)?;
        Ok(true)
    }

    fn latest_chain_for_conversation(
        &self,
        conversation_key: &str,
        route_id: Option<&str>,
    ) -> Result<Vec<HistoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        chain_for_conversation_leaf(&entries, &keys, conversation_key, route_id)
    }

    fn response_conversation_key(&self, response_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .conversation_keys
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?
            .get(response_id)
            .cloned())
    }

    fn conversation_route_exchange_count(
        &self,
        conversation_key: &str,
        route_id: &str,
    ) -> Result<u64, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(entries
            .values()
            .filter(|entry| {
                entry.route_id == route_id
                    && keys.get(&entry.response_id).map(String::as_str) == Some(conversation_key)
            })
            .count() as u64)
    }

    fn get_chain(&self, head: &str) -> Result<Vec<HistoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(walk_chain(&entries, head))
    }

    fn record_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
        generation: u32,
        canonical_items: Vec<Value>,
    ) -> Result<(), String> {
        let source_items = canonical_items.clone();
        let source_hash = hash_items(&source_items);
        let checkpoint_hash = hash_items(&canonical_items);
        self.record_compaction_record(CompactionJournalRecord {
            response_id: response_id.to_string(),
            compaction_id: compaction_id.to_string(),
            generation,
            engine_id: crate::codex_local_v0_150::ENGINE_ID.to_string(),
            engine_provenance: Some(crate::codex_local_v0_150::ENGINE_PROVENANCE.to_string()),
            route_id: String::new(),
            upstream_model: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            elapsed_ms: 0,
            schema_version: LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            source_items,
            canonical_items,
            source_hash,
            checkpoint_hash,
            provider_owned: false,
        })
    }

    fn record_compaction_record(&self, record: CompactionJournalRecord) -> Result<(), String> {
        let response_id = record.response_id.clone();
        let journal = CompactionJournal::from_record(record);
        let entries = self
            .entries
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let mut journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        let conversation_keys = self
            .conversation_keys
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        journals.entry(response_id).or_default().push(journal);
        self.persist_locked(&entries, &journals, &conversation_keys)
    }

    fn journal_record(&self, response_id: &str) -> Result<Option<CompactionJournal>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .and_then(|list| list.last().cloned()))
    }

    fn journal_record_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<CompactionJournal>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .cloned())
    }

    fn compaction_ids_for_response(&self, response_id: &str) -> Result<Vec<String>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .map(|list| {
                list.iter()
                    .map(|journal| journal.compaction_id.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn compaction_generation(&self, compaction_id: &str) -> Result<Option<u32>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .map(|journal| journal.generation))
    }

    fn journal_canonical_items(&self, response_id: &str) -> Result<Option<Vec<Value>>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .get(response_id)
            .and_then(|list| list.last())
            .map(|journal| journal.canonical_items.clone()))
    }

    fn canonical_items_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<Vec<Value>>, String> {
        let journals = self
            .journals
            .lock()
            .map_err(|_| "file history store lock poisoned".to_string())?;
        Ok(journals
            .values()
            .flatten()
            .find(|journal| journal.compaction_id == compaction_id)
            .map(|journal| journal.canonical_items.clone()))
    }
}

/// Flatten a chain plus the current turn's items into one oldest → newest
/// item list (pure function, mirrors Desktop `flatten_chain_items`).
pub fn flatten_chain_items(chain: &[HistoryEntry], current_items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for entry in chain {
        out.extend(entry.input_items.iter().cloned());
        out.extend(entry.output_items.iter().cloned());
    }
    out.extend(current_items.iter().cloned());
    out
}

/// Hydrate `previous_response_id` chain into `body.input` and strip the
/// field. Returns proxy-private provenance; callers must not write it into
/// request JSON.
pub fn hydrate_input(body: &mut Value, chain: &[HistoryEntry]) -> crate::replay::HydrationOutcome {
    crate::replay::hydrate_input_with_mode(
        body,
        chain,
        crate::continuation::ContinuationMode::PortableSemanticReplay,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_response(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": format!("msg_{id}"),
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
                "phase": "final_answer"
            }]
        })
    }

    #[test]
    fn record_and_get_chain_roundtrip_returns_oldest_to_newest() {
        let store = MemoryHistoryStore::new();
        store
            .record_exchange(
                &json!({"model": "m", "input": [{"role": "user", "content": "first"}]}),
                &sample_response("resp_t1", "First reply"),
                "route-1",
            )
            .unwrap();
        store
            .record_exchange(
                &json!({
                    "model": "m",
                    "previous_response_id": "resp_t1",
                    "input": [{"role": "user", "content": "second"}]
                }),
                &sample_response("resp_t2", "Second reply"),
                "route-1",
            )
            .unwrap();

        let chain = store.get_chain("resp_t2").unwrap();
        assert_eq!(chain.len(), 2, "chain must walk both links: {chain:?}");
        assert_eq!(chain[0].response_id, "resp_t1");
        assert_eq!(chain[0].previous_response_id, None);
        assert_eq!(
            chain[0].input_items,
            vec![json!({"role": "user", "content": "first"})]
        );
        assert_eq!(
            chain[0].output_items[0]["content"][0]["text"],
            "First reply"
        );
        assert_eq!(chain[0].route_id, "route-1");
        assert_eq!(chain[1].response_id, "resp_t2");
        assert_eq!(chain[1].previous_response_id.as_deref(), Some("resp_t1"));
        assert_eq!(
            chain[1].input_items,
            vec![json!({"role": "user", "content": "second"})]
        );
    }

    #[test]
    fn record_without_response_id_returns_false_and_records_nothing() {
        let store = MemoryHistoryStore::new();
        let recorded = store
            .record_exchange(
                &json!({"model": "m", "input": "hi"}),
                &json!({"object": "response", "output": []}),
                "route-1",
            )
            .unwrap();
        assert!(!recorded);
        assert!(store.get_chain("anything").unwrap().is_empty());
    }

    #[test]
    fn unknown_head_stops_the_walk_with_an_empty_chain() {
        let store = MemoryHistoryStore::new();
        store
            .record_exchange(
                &json!({"input": "hi"}),
                &sample_response("resp_1", "hi back"),
                "route-1",
            )
            .unwrap();
        assert!(store.get_chain("resp_ghost").unwrap().is_empty());
    }

    #[test]
    fn a_cycle_in_the_chain_cannot_loop_forever() {
        let store = MemoryHistoryStore::new();
        store
            .record_exchange(
                &json!({"previous_response_id": "resp_b", "input": "a"}),
                &sample_response("resp_a", "A"),
                "route-1",
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "resp_a", "input": "b"}),
                &sample_response("resp_b", "B"),
                "route-1",
            )
            .unwrap();
        let chain = store.get_chain("resp_a").unwrap();
        assert!(!chain.is_empty());
        assert!(
            chain.len() <= 2,
            "cycle guard must bound the walk: {chain:?}"
        );
    }

    #[test]
    fn encrypted_reasoning_record_keeps_summary_and_never_ciphertext() {
        // The runtime persists the *normalized* response: ciphertext is
        // stripped before recording, exactly as Desktop does.
        let raw = json!({
            "id": "resp_t1",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_enc_1",
                    "encrypted_content": "opaque-secret",
                    "summary": [{"type": "summary_text", "text": "Thought through the turn."}]
                },
                {
                    "type": "message",
                    "id": "msg_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}]
                }
            ]
        });
        let normalized = crate::response::normalize_non_streaming_response(
            &json!({"model": "m", "input": [{"role": "user", "content": "first turn"}]}),
            crate::route::RuntimeWireFormat::Responses,
            "test-model",
            raw,
        )
        .unwrap();

        let store = MemoryHistoryStore::new();
        store
            .record_exchange(
                &json!({"model": "m", "input": [{"role": "user", "content": "first turn"}]}),
                &normalized,
                "route-1",
            )
            .unwrap();

        let chain = store.get_chain("resp_t1").unwrap();
        assert_eq!(chain.len(), 1);
        let stored = chain[0].output_items.clone();
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(
            !serialized.contains("encrypted_content") && !serialized.contains("opaque-secret"),
            "provider-private ciphertext must never be persisted: {serialized}"
        );
        let reasoning = stored
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
            .expect("reasoning item must survive");
        assert_eq!(reasoning["id"], "rs_enc_1");
        assert_eq!(reasoning["summary"][0]["text"], "Thought through the turn.");
    }

    #[test]
    fn file_store_survives_reopen_for_restart_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        {
            let store = FileHistoryStore::open(&path).unwrap();
            store
                .record_exchange(
                    &json!({"input": [{"role": "user", "content": "first"}]}),
                    &sample_response("resp_t1", "First reply"),
                    "route-1",
                )
                .unwrap();
        }
        // Simulates a process restart: a brand-new store on the same path.
        let reopened = FileHistoryStore::open(&path).unwrap();
        let chain = reopened.get_chain("resp_t1").unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].response_id, "resp_t1");
        assert_eq!(
            chain[0].output_items[0]["content"][0]["text"],
            "First reply"
        );
        assert_eq!(chain[0].route_id, "route-1");
    }

    #[test]
    fn hydrate_input_replays_chain_and_strips_previous_response_id() {
        let store = MemoryHistoryStore::new();
        store
            .record_exchange(
                &json!({"input": [{"role": "user", "content": "first turn"}]}),
                &sample_response("resp_t1", "First reply"),
                "route-1",
            )
            .unwrap();
        let chain = store.get_chain("resp_t1").unwrap();
        let mut body = json!({
            "model": "m",
            "previous_response_id": "resp_t1",
            "input": [{"role": "user", "content": "second turn"}]
        });
        hydrate_input(&mut body, &chain);
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": "first turn"},
                {
                    "type": "message",
                    "id": "msg_resp_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                    "phase": "final_answer"
                },
                {"role": "user", "content": "second turn"}
            ])
        );
    }

    #[test]
    fn hydrate_input_with_empty_chain_restores_input_untouched() {
        let mut body = json!({
            "model": "m",
            "previous_response_id": "resp_ghost",
            "input": [{"role": "user", "content": "second turn"}]
        });
        hydrate_input(&mut body, &[]);
        assert!(body.get("previous_response_id").is_none());
        // Mirrors Desktop: a single-item input stays a single item (not an
        // array) when there is no chain to replay.
        assert_eq!(
            body["input"],
            json!({"role": "user", "content": "second turn"})
        );
    }

    #[test]
    fn hydrate_input_without_input_field_only_strips_previous_response_id() {
        let mut body = json!({"model": "m", "previous_response_id": "resp_t1"});
        hydrate_input(&mut body, &[]);
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("input").is_none());
    }

    #[test]
    fn file_store_persists_compaction_journals_across_reopen() {
        // M6: the durable journal (checkpoint window + generation) must
        // survive a store reopen exactly like exchanges do.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        let store = FileHistoryStore::open(&path).unwrap();
        store
            .record_exchange(
                &json!({"model": "m", "input": []}),
                &json!({"id": "resp_1", "object": "response", "status": "completed", "output": []}),
                "route",
            )
            .unwrap();
        store
            .record_compaction(
                "resp_1",
                "ckpt_1",
                1,
                vec![json!({"type": "message", "role": "user"})],
            )
            .unwrap();
        drop(store);

        let reopened = FileHistoryStore::open(&path).unwrap();
        assert_eq!(
            reopened.compaction_ids_for_response("resp_1").unwrap(),
            vec!["ckpt_1"]
        );
        assert_eq!(reopened.compaction_generation("ckpt_1").unwrap(), Some(1));
        assert_eq!(
            reopened.compacted_replay_items("resp_1").unwrap(),
            Some(vec![json!({"type": "message", "role": "user"})])
        );
        // Exchanges still load alongside journals in the same file.
        assert_eq!(reopened.get_chain("resp_1").unwrap().len(), 1);
    }

    #[test]
    fn latest_chain_for_conversation_matches_on_memory_and_file() {
        let request = json!({
            "prompt_cache_key": "thread-abc",
            "input": [{"role": "user", "content": "first"}]
        });
        let memory = MemoryHistoryStore::new();
        memory
            .record_exchange(
                &request,
                &sample_response("resp_t1", "First reply"),
                "route-1",
            )
            .unwrap();
        memory
            .record_exchange(
                &json!({
                    "prompt_cache_key": "thread-abc",
                    "previous_response_id": "resp_t1",
                    "input": [{"role": "user", "content": "second"}]
                }),
                &sample_response("resp_t2", "Second reply"),
                "route-1",
            )
            .unwrap();
        let key = crate::grok_session::conversation_key_from_raw(Some("thread-abc")).unwrap();
        let chain = memory
            .latest_chain_for_conversation(&key, Some("route-1"))
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].response_id, "resp_t2");
    }

    #[test]
    fn latest_chain_for_conversation_fails_closed_on_ambiguous_leaves() {
        let memory = MemoryHistoryStore::new();
        memory
            .record_exchange_with_conversation_key(
                &json!({"input": [{"role": "user", "content": "a"}]}),
                &sample_response("resp_a", "A"),
                "route-1",
                Some("shared-key"),
            )
            .unwrap();
        memory
            .record_exchange_with_conversation_key(
                &json!({"input": [{"role": "user", "content": "b"}]}),
                &sample_response("resp_b", "B"),
                "route-1",
                Some("shared-key"),
            )
            .unwrap();
        let error = memory
            .latest_chain_for_conversation("shared-key", Some("route-1"))
            .unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn latest_chain_file_roundtrip_continues() {
        let request = json!({
            "prompt_cache_key": "thread-abc",
            "input": [{"role": "user", "content": "first"}]
        });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        {
            let store = FileHistoryStore::open(&path).unwrap();
            store
                .record_exchange(
                    &request,
                    &sample_response("resp_t1", "First reply"),
                    "route-1",
                )
                .unwrap();
            store
                .record_exchange(
                    &json!({
                        "prompt_cache_key": "thread-abc",
                        "previous_response_id": "resp_t1",
                        "input": [{"role": "user", "content": "second"}]
                    }),
                    &sample_response("resp_t2", "Second reply"),
                    "route-1",
                )
                .unwrap();
        }
        let reopened = FileHistoryStore::open(&path).unwrap();
        let key = crate::grok_session::conversation_key_from_raw(Some("thread-abc")).unwrap();
        let file_chain = reopened
            .latest_chain_for_conversation(&key, Some("route-1"))
            .unwrap();
        assert_eq!(file_chain.len(), 2);
        assert_eq!(file_chain[1].response_id, "resp_t2");
    }

    #[test]
    fn parent_and_child_resolve_to_distinct_conversation_keys() {
        let parent_id = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(crate::codex_metadata::CodexOpaqueId::new("s", "S_HIST").unwrap()),
            thread_id: Some(crate::codex_metadata::CodexOpaqueId::new("t", "T_PARENT").unwrap()),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Structured,
            ..Default::default()
        };
        let child_id = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(crate::codex_metadata::CodexOpaqueId::new("s", "S_HIST").unwrap()),
            thread_id: Some(crate::codex_metadata::CodexOpaqueId::new("t", "T_CHILD").unwrap()),
            parent_thread_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("p", "T_PARENT").unwrap(),
            ),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Exact,
            ..Default::default()
        };

        let parent_req = crate::request::RuntimeRequest {
            body: json!({ "model": "gpt-4", "input": [] }),
            endpoint: crate::request::RuntimeEndpoint::Responses,
            incoming_auth: crate::request::IncomingAuthContext::default(),
            execution_environment: crate::environment::ExecutionEnvironment::posix_reference(),
            metadata: crate::request::RequestMetadata {
                request_id: "req_p".into(),
                received_at_ms: 1000,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                codex_identity: Some(parent_id),
                codex_capabilities: None,
                force_portable_official_handoff: false,
                review_official_account_id: None,
            },
        };

        let child_req = crate::request::RuntimeRequest {
            body: json!({ "model": "gpt-4", "input": [] }),
            endpoint: crate::request::RuntimeEndpoint::Responses,
            incoming_auth: crate::request::IncomingAuthContext::default(),
            execution_environment: crate::environment::ExecutionEnvironment::posix_reference(),
            metadata: crate::request::RequestMetadata {
                request_id: "req_c".into(),
                received_at_ms: 1000,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                codex_identity: Some(child_id),
                codex_capabilities: None,
                force_portable_official_handoff: false,
                review_official_account_id: None,
            },
        };

        let res_p = resolve_conversation_key(&parent_req, None);
        let res_c = resolve_conversation_key(&child_req, None);

        assert_eq!(res_p.key, "codex:S_HIST:T_PARENT");
        assert_eq!(res_p.source, ConversationKeySource::CodexThreadIdentity);
        assert_eq!(res_c.key, "codex:S_HIST:T_CHILD");
        assert_eq!(res_c.source, ConversationKeySource::CodexThreadIdentity);
        assert_ne!(res_p.key, res_c.key);
    }

    #[test]
    fn same_thread_across_context_window_rollover_keeps_same_conversation_key() {
        let turn1 = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(crate::codex_metadata::CodexOpaqueId::new("s", "S_HIST").unwrap()),
            thread_id: Some(crate::codex_metadata::CodexOpaqueId::new("t", "T_CHILD").unwrap()),
            context_window_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("cw", "CW1").unwrap(),
            ),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Exact,
            ..Default::default()
        };
        let turn2 = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(crate::codex_metadata::CodexOpaqueId::new("s", "S_HIST").unwrap()),
            thread_id: Some(crate::codex_metadata::CodexOpaqueId::new("t", "T_CHILD").unwrap()),
            context_window_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("cw", "CW2").unwrap(),
            ),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Exact,
            ..Default::default()
        };

        let key1 = codex_conversation_key(&turn1).unwrap();
        let key2 = codex_conversation_key(&turn2).unwrap();

        assert_eq!(key1, "codex:S_HIST:T_CHILD");
        assert_eq!(key2, "codex:S_HIST:T_CHILD");
        assert_eq!(key1, key2);
    }

    #[test]
    fn opaque_session_and_thread_ids_have_collision_free_conversation_keys() {
        let identity = |session: &str, thread: &str| crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("session_id", session).unwrap(),
            ),
            thread_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("thread_id", thread).unwrap(),
            ),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Structured,
            ..Default::default()
        };

        let left = codex_conversation_key(&identity("a:b", "c")).unwrap();
        let right = codex_conversation_key(&identity("a", "b:c")).unwrap();
        let literal_escape = codex_conversation_key(&identity("a%3Ab", "c")).unwrap();

        assert_eq!(left, "codex:a%3Ab:c");
        assert_eq!(right, "codex:a:b%3Ac");
        assert_eq!(literal_escape, "codex:a%253Ab:c");
        assert_ne!(left, right);
        assert_ne!(left, literal_escape);
    }

    #[test]
    fn conflicting_identity_never_rekeys_existing_history() {
        let conf_id = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(crate::codex_metadata::CodexOpaqueId::new("s", "S_CONF").unwrap()),
            thread_id: Some(crate::codex_metadata::CodexOpaqueId::new("t", "T_CONF").unwrap()),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Conflict,
            ..Default::default()
        };

        assert!(codex_conversation_key(&conf_id).is_none());

        let req = crate::request::RuntimeRequest {
            body: json!({ "prompt_cache_key": "safe-existing-key" }),
            endpoint: crate::request::RuntimeEndpoint::Responses,
            incoming_auth: crate::request::IncomingAuthContext::default(),
            execution_environment: crate::environment::ExecutionEnvironment::posix_reference(),
            metadata: crate::request::RequestMetadata {
                request_id: "req_conf".into(),
                received_at_ms: 1000,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                codex_identity: Some(conf_id),
                codex_capabilities: None,
                force_portable_official_handoff: false,
                review_official_account_id: None,
            },
        };

        let res_with_fallback = resolve_conversation_key(&req, Some("safe-existing-key"));
        assert_eq!(res_with_fallback.key, "safe-existing-key");
        assert_eq!(
            res_with_fallback.source,
            ConversationKeySource::ExistingConversationKey
        );

        let res_without_fallback = resolve_conversation_key(&req, None);
        assert_eq!(
            res_without_fallback.source,
            ConversationKeySource::ExistingConversationKey
        );
        assert!(!res_without_fallback.key.starts_with("codex:"));
    }

    #[test]
    fn metadata_absent_preserves_existing_legacy_conversation_resolution() {
        let req = crate::request::RuntimeRequest {
            body: json!({ "previous_response_id": "resp_prev_123" }),
            endpoint: crate::request::RuntimeEndpoint::Responses,
            incoming_auth: crate::request::IncomingAuthContext::default(),
            execution_environment: crate::environment::ExecutionEnvironment::posix_reference(),
            metadata: crate::request::RequestMetadata {
                request_id: "req_no_meta".into(),
                received_at_ms: 1000,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                codex_identity: None,
                codex_capabilities: None,
                force_portable_official_handoff: false,
                review_official_account_id: None,
            },
        };

        let res = resolve_conversation_key(&req, None);
        assert_eq!(res.key, "chain:resp_prev_123");
        assert_eq!(res.source, ConversationKeySource::PreviousResponseChain);
    }

    #[test]
    fn legacy_audit_json_without_breakdown_fields_deserializes_as_none_not_zero() {
        let legacy_json = r#"{
            "schemaVersion": 2,
            "sourceHash": "abc",
            "checkpointHash": "def",
            "sourceItemCount": 10,
            "sourceTokens": 1000,
            "checkpointTokens": 500,
            "semanticClaimCount": 5,
            "groundedClaimCount": 5,
            "rejectedClaimCount": 0,
            "priorCheckpointUsed": false,
            "repeatedSequencesDetected": 0,
            "repeatedExchangesCollapsed": 0,
            "softTrimmedOutputs": 0,
            "hardClearedOutputs": 0,
            "extractionAttempts": 1,
            "fallbackUsed": false
        }"#;

        let record: CanonicalAuditRecord = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(record.source_tokens, 1000);
        assert_eq!(record.checkpoint_tokens, 500);
        assert_eq!(
            record.source_model_visible_tokens, None,
            "Legacy missing breakdown field must be None"
        );
        assert_eq!(
            record.replacement_model_visible_tokens, None,
            "Legacy missing breakdown field must be None"
        );
        assert_eq!(
            record.replacement_durable_tokens, None,
            "Legacy missing breakdown field must be None"
        );
        assert_eq!(
            record.model_visible_compression_ratio, None,
            "Legacy missing breakdown field must be None"
        );
    }
}
