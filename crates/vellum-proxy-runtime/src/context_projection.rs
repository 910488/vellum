//! Model-visible projection.
//!
//! What remains here after the Codex Local Compact 0.150 switchover is the
//! projection used to strip Vellum's local persistence metadata before a
//! window is shown to a model or compared in an eval, plus the token
//! breakdown carried on compaction diagnostics. The Canonical checkpoint
//! projection, quality scoring, and markdown rendering are gone with the
//! engine they described.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token accounting for one compaction, split by what the model actually sees
/// versus what Vellum stores durably.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalTokenBreakdown {
    pub source_model_visible_tokens: u64,
    pub summary_model_visible_tokens: u64,
    pub tail_model_visible_tokens: u64,
    pub replacement_model_visible_tokens: u64,
    pub replacement_durable_tokens: u64,
    pub model_visible_compression_ratio: f64,
    #[serde(default)]
    pub tool_results_pruned: usize,
    #[serde(default)]
    pub pruned_chars_removed: usize,
    #[serde(default)]
    pub pruned_estimated_tokens_removed: u64,
}

/// Project model-visible items by removing Vellum local persistence metadata.
///
/// Specifically strips Vellum checkpoint metadata (`vellum_checkpoint`, `checkpoint`, etc.)
/// and runtime internal fields, while preserving non-Vellum metadata and standard content.
pub fn project_model_visible_item(item: &Value) -> Value {
    let mut cloned = item.clone();
    if let Some(obj) = cloned.as_object_mut() {
        if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
            let is_vellum = meta.get("vellum_checkpoint").and_then(Value::as_str)
                == Some("canonical")
                || meta.get("checkpoint").and_then(Value::as_str) == Some("canonical")
                || (meta.contains_key("vellum_checkpoint")
                    && (meta.contains_key("schema_version")
                        || meta.contains_key("checkpoint_hash")));
            if is_vellum {
                meta.remove("vellum_checkpoint");
                meta.remove("checkpoint");
                meta.remove("schema_version");
                meta.remove("checkpoint_schema_version");
                meta.remove("continuity_kind");
                meta.remove("retained_tail_count");
                meta.remove("source_hash");
                meta.remove("checkpoint_hash");
                if meta.is_empty() {
                    obj.remove("metadata");
                }
            }
        }
        obj.remove("internal_compaction_parent");
        obj.remove("internal_replay_parent");
        obj.remove("internal_provenance_id");
    }
    cloned
}

/// Project a slice of items into model-visible items without local metadata.
pub fn project_model_visible_items(items: &[Value]) -> Vec<Value> {
    items.iter().map(project_model_visible_item).collect()
}
