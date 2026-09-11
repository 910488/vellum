//! Engine-neutral compaction resolution.
//!
//! After the Codex Local Compact 0.150 switchover there are exactly two
//! outcomes, and neither is selectable by the user:
//!
//! * `OfficialNative` — OpenAI Official keeps its own native compaction.
//!   Vellum does not intercept the trigger, does not change the endpoint, and
//!   does not rewrite the body.
//! * `CodexLocalV0150` — every third-party route (Grok, OpenAI-compatible)
//!   and Review compacts with the embedded Codex 0.150 engine.
//!
//! There is deliberately no compactor route selector, no global compactor, no
//! Grok-native path, and no production "disabled" strategy: a route that could
//! silently stop compacting, or compact on some other model, is exactly the
//! class of drift this switchover removes. Resolution is a pure function of
//! the route's provider kind so it cannot be overridden per session.

use serde::{Deserialize, Serialize};

use crate::codex_local_v0_150;
use crate::route::{RuntimeModelRoute, RuntimeProviderKind};

/// The engine that will handle compaction for a route. Read-only: surfaced to
/// the UI and API, never accepted as an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedCompactionEngine {
    /// OpenAI Official native compaction, passed through untouched.
    OfficialNative,
    /// Vellum's embedded port of Codex 0.150 local compaction.
    CodexLocalV0150,
}

impl ResolvedCompactionEngine {
    /// Stable wire/API identifier.
    pub fn id(self) -> &'static str {
        match self {
            Self::OfficialNative => "official_native",
            Self::CodexLocalV0150 => codex_local_v0_150::ENGINE_ID,
        }
    }

    /// Human-facing label.
    pub fn label(self) -> &'static str {
        match self {
            Self::OfficialNative => "OpenAI Official (native)",
            Self::CodexLocalV0150 => codex_local_v0_150::ENGINE_LABEL,
        }
    }

    /// Upstream provenance for the engine, where Vellum owns one.
    pub fn provenance(self) -> Option<&'static str> {
        match self {
            Self::OfficialNative => None,
            Self::CodexLocalV0150 => Some(codex_local_v0_150::ENGINE_PROVENANCE),
        }
    }

    /// Whether Vellum materializes the compaction itself. Official does not
    /// reach any Vellum compaction code path.
    pub fn is_vellum_local(self) -> bool {
        matches!(self, Self::CodexLocalV0150)
    }
}

/// Resolve the production compaction engine for a route.
///
/// Official is byte-preserving passthrough; everything else — Grok,
/// OpenAI-compatible, and Review routes, which run on third-party providers —
/// uses the embedded Codex 0.150 engine.
pub fn resolve_compaction_engine(route: &RuntimeModelRoute) -> ResolvedCompactionEngine {
    match route.provider_kind {
        RuntimeProviderKind::Official => ResolvedCompactionEngine::OfficialNative,
        _ => ResolvedCompactionEngine::CodexLocalV0150,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_with(kind: RuntimeProviderKind) -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-1".into(),
            catalog_id: "vlm-sample".into(),
            name: "Sample".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: kind,
            auth_kind: crate::route::RuntimeAuthKind::Bearer,
            wire: crate::route::RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "sample-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: Some("sample-credential".into()),
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: Default::default(),
        }
    }

    #[test]
    fn official_resolves_to_native_passthrough() {
        let engine = resolve_compaction_engine(&route_with(RuntimeProviderKind::Official));
        assert_eq!(engine, ResolvedCompactionEngine::OfficialNative);
        assert!(!engine.is_vellum_local());
        assert_eq!(engine.provenance(), None);
    }

    #[test]
    fn grok_cli_resolves_to_codex_local() {
        let engine = resolve_compaction_engine(&route_with(RuntimeProviderKind::GrokCli));
        assert_eq!(engine, ResolvedCompactionEngine::CodexLocalV0150);
        assert!(engine.is_vellum_local());
        assert_eq!(engine.id(), "codex_local_v0_150");
        assert_eq!(
            engine.provenance(),
            Some("openai-codex@rust-v0.150.0-alpha.8+fcbdb578")
        );
    }

    #[test]
    fn openai_compatible_resolves_to_codex_local() {
        let engine = resolve_compaction_engine(&route_with(RuntimeProviderKind::OpenAiCompatible));
        assert_eq!(engine, ResolvedCompactionEngine::CodexLocalV0150);
    }

    #[test]
    fn engine_ids_are_the_only_two_public_values() {
        assert_eq!(
            ResolvedCompactionEngine::OfficialNative.id(),
            "official_native"
        );
        assert_eq!(
            ResolvedCompactionEngine::CodexLocalV0150.id(),
            "codex_local_v0_150"
        );
    }
}

#[cfg(test)]
mod record_tests {
    use crate::history::{hash_items, LocalCompactionRecordV1, LOCAL_COMPACTION_RECORD_SCHEMA_V1};
    use crate::replay::{
        is_codex_local_v0_150_marker, is_legacy_canonical_marker, reject_legacy_canonical_history,
        CODEX_LOCAL_V0_150_MARKER_PREFIX, LEGACY_CANONICAL_UNSUPPORTED,
    };
    use serde_json::{json, Value};

    fn items(text: &str) -> Vec<Value> {
        vec![json!({"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]})]
    }

    fn record() -> LocalCompactionRecordV1 {
        let source = items("source");
        let replacement = items("replacement");
        LocalCompactionRecordV1 {
            schema_version: LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            response_id: "resp_1".into(),
            compaction_id: format!("{CODEX_LOCAL_V0_150_MARKER_PREFIX}abc"),
            generation: 1,
            engine_id: crate::codex_local_v0_150::ENGINE_ID.into(),
            engine_provenance: Some(crate::codex_local_v0_150::ENGINE_PROVENANCE.into()),
            source_hash: hash_items(&source),
            source_items: source,
            replacement_hash: hash_items(&replacement),
            replacement_items: replacement,
            route_id: "route-1".into(),
            upstream_model: "grok-4.6".into(),
            tokens_before: 100_000,
            tokens_after: 12_000,
            elapsed_ms: 4_200,
            failure: None,
            created_at: 0,
        }
    }

    #[test]
    fn a_well_formed_record_validates() {
        assert_eq!(record().validate(), Ok(()));
    }

    #[test]
    fn a_tampered_source_fails_closed() {
        let mut r = record();
        r.source_items = items("something else");
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_tampered_replacement_fails_closed() {
        let mut r = record();
        r.replacement_items = items("something else");
        assert!(r.validate().is_err());
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        let mut r = record();
        r.schema_version = 99;
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let r = record();
        let encoded = serde_json::to_string(&r).expect("encode");
        let decoded: LocalCompactionRecordV1 = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, r);
        assert_eq!(decoded.validate(), Ok(()));
    }

    #[test]
    fn markers_are_engine_specific() {
        let ours = json!({"type": "compaction",
                          "encrypted_content": format!("{CODEX_LOCAL_V0_150_MARKER_PREFIX}x")});
        let legacy = json!({"type": "compaction", "encrypted_content": "vcomp1.x"});
        assert!(is_codex_local_v0_150_marker(&ours));
        assert!(!is_codex_local_v0_150_marker(&legacy));
        assert!(is_legacy_canonical_marker(&legacy));
        assert!(!is_legacy_canonical_marker(&ours));
    }

    #[test]
    fn legacy_canonical_history_is_rejected_by_name() {
        let legacy = vec![json!({"type": "compaction", "encrypted_content": "vcomp1.x"})];
        assert_eq!(
            reject_legacy_canonical_history(&legacy),
            Err(LEGACY_CANONICAL_UNSUPPORTED)
        );
        // The marker is never simply dropped so the turn can proceed.
        assert!(reject_legacy_canonical_history(&items("ordinary")).is_ok());
    }
}

#[cfg(test)]
mod journal_engine_tests {
    use crate::history::{CompactionJournal, CompactionJournalRecord};

    fn record(engine_id: &str) -> CompactionJournalRecord {
        CompactionJournalRecord {
            response_id: "resp_1".into(),
            compaction_id: "cmp_vellum_abc".into(),
            generation: 1,
            engine_id: engine_id.into(),
            engine_provenance: None,
            route_id: "route-1".into(),
            upstream_model: "grok-4.6".into(),
            tokens_before: 100_000,
            tokens_after: 12_000,
            elapsed_ms: 4_200,
            schema_version: crate::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            source_items: Vec::new(),
            canonical_items: Vec::new(),
            source_hash: String::new(),
            checkpoint_hash: String::new(),
            provider_owned: false,
        }
    }

    #[test]
    fn a_record_carries_the_engine_that_wrote_it() {
        let journal = CompactionJournal::from_record(record("codex_local_v0_150"));
        assert_eq!(journal.engine_id, "codex_local_v0_150");
        assert_eq!(journal.upstream_model, "grok-4.6");
        assert_eq!(journal.tokens_before, 100_000);
    }

    #[test]
    fn a_record_written_before_the_switchover_decodes_as_the_retired_engine() {
        // Rows persisted by the Canonical engine have no engineId at all. They
        // must decode to something that is visibly not the current engine, so
        // materialization refuses them instead of installing their items.
        let legacy = serde_json::json!({
            "compaction_id": "cmp_vellum_old",
            "response_id": "resp_old",
            "generation": 1,
            "canonical_items": [],
            "created_at": 0
        });
        let journal: CompactionJournal = serde_json::from_value(legacy).expect("decode");
        assert_ne!(journal.engine_id, "codex_local_v0_150");
        assert_eq!(journal.engine_id, "vellum_canonical_retired");
    }
}
