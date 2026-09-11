//! Shared Grok session and turn-index continuity.
//!
//! Grok session/request bookkeeping used to be forked per caller: Desktop's
//! now-dead `proxy_legacy.rs` router carried its own `GrokSessionRegistry`
//! (test-only fixture code since the M11 cutover — `src-tauri/src/proxy.rs`
//! compiles it only under `cfg(test)`), while the production path
//! (`ResolvedAuth::resolve` in `auth.rs`) carried a stateless M3C stub:
//! session id from a bare `body.session` field, a per-call request-id
//! fingerprint with no retry dedup, and `turn_index` hardcoded to `Some(0)`
//! on every call. This module replaces both with one implementation that
//! Desktop (`src-tauri`) and the headless daemon (`vellum-proxy-daemon`,
//! which assembles the exact same [`crate::exec::ProxyRuntime`]) dispatch
//! through, so retries reuse a request id and turn index, and a restart
//! seeds the turn index from durable history instead of forking a new one
//! from zero.
//!
//! This registry resolves *turn* identity (session id, request id, turn
//! index) for both normal conversation turns and Grok Build's auxiliary
//! compaction/summarizer calls. Compaction routes through
//! [`GrokSessionRegistry::compaction_identity`] instead of
//! [`GrokSessionRegistry::request_identity`]: same session, no turn index, a
//! distinct `xai-compact-*` request id that a retry of the identical
//! auxiliary body reuses rather than regenerates.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::RuntimeError;
use crate::history::HistoryStore;

const GROK_SESSION_LRU_LIMIT: usize = 4096;

/// One resolved Grok request identity: which session it belongs to, its
/// stable request id (same value on a retry of the same body), and its turn
/// index (`None` only for a compaction/auxiliary call, never for a normal
/// agent turn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrokRequestIdentity {
    pub session_id: String,
    /// Normal agent turns carry an index. Grok Build's auxiliary compaction
    /// sampler deliberately omits `x-grok-turn-idx`.
    pub turn_index: Option<u64>,
    pub request_id: String,
}

/// Shared Grok session/turn-index registry. Transient by design — it is
/// rebuilt empty on every process start; durable continuity across a restart
/// comes from seeding the turn index off `HistoryStore` (see
/// [`GrokSessionRegistry::request_identity`]), not from persisting this
/// registry itself.
#[derive(Default)]
pub struct GrokSessionRegistry {
    inner: Mutex<GrokSessionState>,
}

#[derive(Default)]
struct GrokSessionState {
    response_sessions: HashMap<String, String>,
    response_session_order: VecDeque<String>,
    turn_indexes: HashMap<String, u64>,
    turn_index_order: VecDeque<String>,
    request_identities: HashMap<(String, String), GrokRequestIdentity>,
    request_identity_order: VecDeque<(String, String)>,
}

impl GrokSessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve session identity with optional durable history fallback when
    /// the in-memory response→session map misses (restart / LRU eviction). A
    /// real history store error fails closed instead of treating the lookup
    /// failure as a miss and forking a new session identity.
    pub fn resolve_session(
        &self,
        request: &Value,
        history: Option<&dyn HistoryStore>,
    ) -> Result<String, RuntimeError> {
        if let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            {
                let state = self
                    .inner
                    .lock()
                    .map_err(|_| RuntimeError::Internal("grok session registry poisoned".into()))?;
                if let Some(session) = state.response_sessions.get(previous) {
                    return Ok(session.clone());
                }
            }
            if let Some(store) = history {
                match store.response_conversation_key(previous) {
                    Ok(Some(key)) => {
                        self.bind_response(previous, &key);
                        return Ok(key);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(RuntimeError::History(format!(
                            "durable conversation_key lookup failed for previous_response_id \
                             (fail closed, refusing session fork): {error}"
                        )));
                    }
                }
            }
        }
        if let Some(key) = conversation_key_from_request(request) {
            return Ok(key);
        }
        Ok(format!("vellum-grok-{}", random_hex_id()))
    }

    /// Resolve (or reuse) the request identity for one Grok turn on an
    /// already-resolved `session`. Retries of the identical request body on
    /// the same session return the exact same identity (turn index
    /// included) instead of advancing the counter again.
    pub fn request_identity(
        &self,
        session: &str,
        request: &Value,
        history: Option<&dyn HistoryStore>,
        route_id: Option<&str>,
    ) -> Result<GrokRequestIdentity, RuntimeError> {
        let fingerprint = grok_request_fingerprint(session, request);
        let key = (session.to_string(), fingerprint.clone());
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RuntimeError::Internal("grok session registry poisoned".into()))?;
        if let Some(identity) = state.request_identities.get(&key) {
            return Ok(identity.clone());
        }
        if !state.turn_indexes.contains_key(session) {
            let durable_seed = match (history, route_id) {
                (Some(store), Some(route_id)) => store
                    .conversation_route_exchange_count(session, route_id)
                    .map_err(|error| {
                        RuntimeError::History(format!(
                            "durable conversation-route exchange count failed \
                             (fail closed, refusing turn-index fork): {error}"
                        ))
                    })?,
                _ => 0,
            };
            state.turn_indexes.insert(session.to_string(), durable_seed);
            state.turn_index_order.push_back(session.to_string());
            while state.turn_index_order.len() > GROK_SESSION_LRU_LIMIT {
                if let Some(expired) = state.turn_index_order.pop_front() {
                    state.turn_indexes.remove(&expired);
                }
            }
        }
        let next = state.turn_indexes.entry(session.to_string()).or_insert(0);
        let turn_index = *next;
        *next = next.saturating_add(1);
        let identity = GrokRequestIdentity {
            session_id: session.to_string(),
            turn_index: Some(turn_index),
            request_id: format!("vellum-{}", &fingerprint[..32]),
        };
        state
            .request_identities
            .insert(key.clone(), identity.clone());
        state.request_identity_order.push_back(key);
        while state.request_identity_order.len() > GROK_SESSION_LRU_LIMIT {
            if let Some(expired) = state.request_identity_order.pop_front() {
                state.request_identities.remove(&expired);
            }
        }
        Ok(identity)
    }

    /// Grok Build compaction uses the same conversation/session identity,
    /// but is an auxiliary model call rather than a user turn: no turn index,
    /// a distinct `xai-compact-*` request-id prefix instead of the normal
    /// `vellum-*` one.
    ///
    /// `identity_seed` fingerprints the auxiliary call's own outgoing body
    /// (not the enclosing turn's body — several distinct auxiliary calls,
    /// e.g. one per compaction chunk, can share one session within a single
    /// turn and must not collide on request id). A retry that resends the
    /// identical `identity_seed` on the same `session` gets back the exact
    /// same identity instead of a fresh random one each attempt — mirroring
    /// the retry-dedup [`Self::request_identity`] already gives normal
    /// turns.
    pub fn compaction_identity(
        &self,
        session: &str,
        identity_seed: &Value,
    ) -> Result<GrokRequestIdentity, RuntimeError> {
        let fingerprint = grok_request_fingerprint(session, identity_seed);
        let key = (session.to_string(), format!("compaction:{fingerprint}"));
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RuntimeError::Internal("grok session registry poisoned".into()))?;
        if let Some(identity) = state.request_identities.get(&key) {
            return Ok(identity.clone());
        }
        let identity = GrokRequestIdentity {
            session_id: session.to_string(),
            turn_index: None,
            request_id: format!("xai-compact-{}", &fingerprint[..32]),
        };
        state
            .request_identities
            .insert(key.clone(), identity.clone());
        state.request_identity_order.push_back(key);
        while state.request_identity_order.len() > GROK_SESSION_LRU_LIMIT {
            if let Some(expired) = state.request_identity_order.pop_front() {
                state.request_identities.remove(&expired);
            }
        }
        Ok(identity)
    }

    /// Resolve a full request identity for one Grok turn: session + request
    /// id + turn index, with retry/restart continuity built in.
    /// `compaction: true` requests [`Self::compaction_identity`] (fingerprinted
    /// on `request` itself) instead of a normal turn. Callers that need a
    /// compaction identity fingerprinted on something other than `request`
    /// (e.g. a per-chunk auxiliary body distinct from the enclosing turn)
    /// should call [`Self::resolve_session`] and [`Self::compaction_identity`]
    /// directly instead of this convenience wrapper.
    pub fn resolve_turn(
        &self,
        request: &Value,
        history: Option<&dyn HistoryStore>,
        route_id: &str,
        compaction: bool,
    ) -> Result<GrokRequestIdentity, RuntimeError> {
        let session_id = self.resolve_session(request, history)?;
        if compaction {
            return self.compaction_identity(&session_id, request);
        }
        self.request_identity(&session_id, request, history, Some(route_id))
    }

    /// Bind an upstream `response_id` to the session that produced it, so a
    /// later turn carrying `previous_response_id` resolves the same session
    /// even after this registry's own request-identity entry has been
    /// evicted.
    pub fn bind_response(&self, response_id: &str, session: &str) {
        if response_id.is_empty() || session.is_empty() {
            return;
        }
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        if state
            .response_sessions
            .insert(response_id.to_string(), session.to_string())
            .is_none()
        {
            state
                .response_session_order
                .push_back(response_id.to_string());
        }
        while state.response_session_order.len() > GROK_SESSION_LRU_LIMIT {
            if let Some(expired) = state.response_session_order.pop_front() {
                state.response_sessions.remove(&expired);
            }
        }
    }
}

/// Conversation identity carried in the request body itself, mirroring
/// Desktop's `crate::history::conversation_key` (checked in the same
/// priority order) plus a bare `session` field as the final fallback — the
/// field the pre-registry M3C stub in `auth.rs` read exclusively, kept here
/// so an already-deployed caller that only ever set `session` keeps
/// resolving the same identity.
pub fn conversation_key_from_request(request: &Value) -> Option<String> {
    conversation_key_from_raw(
        request
            .get("conversation_id")
            .and_then(Value::as_str)
            .or_else(|| request.get("thread_id").and_then(Value::as_str))
            .or_else(|| request.get("prompt_cache_key").and_then(Value::as_str))
            .or_else(|| {
                request
                    .pointer("/metadata/thread_id")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                request
                    .pointer("/metadata/conversation_id")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                request
                    .pointer("/metadata/session_id")
                    .and_then(Value::as_str)
            })
            .or_else(|| request.get("session").and_then(Value::as_str)),
    )
}

/// Hash a raw conversation/session identifier the same way as
/// [`conversation_key_from_request`].
pub fn conversation_key_from_raw(raw: Option<&str>) -> Option<String> {
    raw.filter(|value| !value.trim().is_empty())
        .map(|value| format!("{:x}", Sha256::digest(value.trim().as_bytes())))
}

/// Stable per-request identity for the Grok header set: the session and the
/// request bytes, hashed. A retry of the identical body on the same session
/// produces the identical fingerprint, which is what lets
/// [`GrokSessionRegistry::request_identity`] dedupe retries.
pub fn grok_request_fingerprint(session: &str, request: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update([0]);
    match serde_json::to_vec(request) {
        Ok(bytes) => hasher.update(bytes),
        Err(_) => hasher.update(request.to_string().as_bytes()),
    }
    format!("{:x}", hasher.finalize())
}

fn random_hex_id() -> String {
    let mut random = [0u8; 16];
    if getrandom::fill(&mut random).is_err() {
        return ulid::Ulid::new().to_string();
    }
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{HistoryEntry, MemoryHistoryStore};
    use serde_json::json;

    #[test]
    fn same_conversation_reuses_session_and_increments_turn() {
        let registry = GrokSessionRegistry::new();
        let first = json!({"model": "grok-4.6", "prompt_cache_key": "thread-a", "input": "one"});
        let second = json!({"model": "grok-4.6", "prompt_cache_key": "thread-a", "input": "two"});
        let turn0 = registry
            .resolve_turn(&first, None, "grok-cli", false)
            .unwrap();
        let turn1 = registry
            .resolve_turn(&second, None, "grok-cli", false)
            .unwrap();
        assert_eq!(turn0.session_id, turn1.session_id);
        assert_eq!(turn0.turn_index, Some(0));
        assert_eq!(turn1.turn_index, Some(1));
        assert_ne!(turn0.request_id, turn1.request_id);
    }

    #[test]
    fn retry_reuses_request_id_and_turn_index() {
        let registry = GrokSessionRegistry::new();
        let body = json!({"model": "grok-4.6", "prompt_cache_key": "thread-b", "input": "same"});
        let first = registry
            .resolve_turn(&body, None, "grok-cli", false)
            .unwrap();
        let retry = registry
            .resolve_turn(&body, None, "grok-cli", false)
            .unwrap();
        assert_eq!(first.request_id, retry.request_id);
        assert_eq!(first.turn_index, retry.turn_index);
        assert_eq!(first.session_id, retry.session_id);
    }

    #[test]
    fn restart_seeds_turn_index_from_durable_history() {
        let history = MemoryHistoryStore::new();
        let session = conversation_key_from_raw(Some("thread-c")).unwrap();
        history
            .record_exchange_with_conversation_key(
                &json!({"prompt_cache_key": "thread-c", "input": "prior"}),
                &json!({"id": "resp_prior", "object": "response", "status": "completed", "output": []}),
                "grok-cli",
                Some(&session),
            )
            .unwrap();
        let registry = GrokSessionRegistry::new();
        let turn = registry
            .resolve_turn(
                &json!({"model": "grok-4.6", "prompt_cache_key": "thread-c", "input": "next"}),
                Some(&history),
                "grok-cli",
                false,
            )
            .unwrap();
        assert_eq!(turn.session_id, session);
        assert_eq!(turn.turn_index, Some(1));
    }

    #[test]
    fn durable_history_error_fails_closed() {
        struct Broken;
        impl HistoryStore for Broken {
            fn record_exchange(
                &self,
                _request: &Value,
                _response: &Value,
                _route_id: &str,
            ) -> Result<bool, String> {
                Ok(false)
            }
            fn get_chain(&self, _head: &str) -> Result<Vec<HistoryEntry>, String> {
                Ok(Vec::new())
            }
            fn response_conversation_key(
                &self,
                _response_id: &str,
            ) -> Result<Option<String>, String> {
                Err("sqlite exploded".into())
            }
        }
        let registry = GrokSessionRegistry::new();
        let error = registry
            .resolve_session(
                &json!({"previous_response_id": "resp_missing"}),
                Some(&Broken),
            )
            .unwrap_err();
        assert!(error.to_string().contains("fail closed"));
        assert!(matches!(error, RuntimeError::History(_)));
    }

    #[test]
    fn compaction_keeps_session_and_omits_turn_index() {
        let registry = GrokSessionRegistry::new();
        let body = json!({"model": "grok-4.6", "prompt_cache_key": "thread-d"});
        let turn = registry
            .resolve_turn(&body, None, "grok-cli", false)
            .unwrap();
        let compact = registry
            .resolve_turn(&body, None, "grok-cli", true)
            .unwrap();
        assert_eq!(turn.session_id, compact.session_id);
        assert_eq!(compact.turn_index, None);
        assert!(compact.request_id.starts_with("xai-compact-"));
        assert_ne!(turn.request_id, compact.request_id);
    }

    #[test]
    fn compaction_retry_reuses_the_same_auxiliary_request_id() {
        let registry = GrokSessionRegistry::new();
        let session = registry
            .resolve_session(&json!({"prompt_cache_key": "thread-retry"}), None)
            .unwrap();
        let seed = json!({"chunk": ["a", "b"], "purpose": "checkpoint"});
        let first_attempt = registry.compaction_identity(&session, &seed).unwrap();
        let retry_attempt = registry.compaction_identity(&session, &seed).unwrap();
        assert_eq!(first_attempt.request_id, retry_attempt.request_id);
        assert_eq!(first_attempt.session_id, retry_attempt.session_id);
        assert_eq!(retry_attempt.turn_index, None);
    }

    #[test]
    fn compaction_identity_distinguishes_different_auxiliary_calls_on_one_session() {
        // Grok Build compaction and the canonical chunked summarizer can both
        // run within one turn on the same session; each distinct chunk/call
        // must get its own request id even though the session is shared.
        let registry = GrokSessionRegistry::new();
        let session = registry
            .resolve_session(&json!({"prompt_cache_key": "thread-multi"}), None)
            .unwrap();
        let chunk_one = registry
            .compaction_identity(&session, &json!({"chunk": 1}))
            .unwrap();
        let chunk_two = registry
            .compaction_identity(&session, &json!({"chunk": 2}))
            .unwrap();
        assert_eq!(chunk_one.session_id, chunk_two.session_id);
        assert_ne!(chunk_one.request_id, chunk_two.request_id);
    }

    #[test]
    fn bare_session_field_is_used_when_no_other_identity_is_present() {
        // The pre-registry `auth.rs` stub read only `body.session`; keeping
        // it as the final fallback means a caller that never migrated to
        // `prompt_cache_key`/`metadata.*` still resolves one stable session.
        let registry = GrokSessionRegistry::new();
        let first = registry
            .resolve_session(&json!({"model": "grok-4.6", "session": "sess-1"}), None)
            .unwrap();
        let second = registry
            .resolve_session(&json!({"model": "grok-4.6", "session": "sess-1"}), None)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first, conversation_key_from_raw(Some("sess-1")).unwrap());
    }

    #[test]
    fn previous_response_id_reuses_the_bound_session_in_memory() {
        let registry = GrokSessionRegistry::new();
        registry.bind_response("resp_1", "session-xyz");
        let session = registry
            .resolve_session(&json!({"previous_response_id": "resp_1"}), None)
            .unwrap();
        assert_eq!(session, "session-xyz");
    }
}
