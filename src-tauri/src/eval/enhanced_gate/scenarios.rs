//! The deterministic catalog the integration gate hands to its children.
//!
//! Real Codex reads a model catalog; the gate writes one. Everything the child
//! is allowed to know about a model — context window, compaction threshold, and
//! which scripted provider behaviour it will meet — lives here, so no code path
//! ever has to infer a port from a model name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const GATE_CATALOG_FILE: &str = "gate-catalog.json";
pub const GATE_CATALOG_ENV: &str = "VELLUM_GATE_CATALOG";
pub const GATE_PROVIDER_URL_ENV: &str = "VELLUM_GATE_PROVIDER_URL";
pub const GATE_WORKSPACE_ENV: &str = "VELLUM_GATE_WORKSPACE";

/// What the scripted provider will do for a model, chosen per case rather than
/// derived from the model's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateScenario {
    /// One assistant message, no tools. Used for Official isolation and for
    /// routing smoke tests.
    PlainAnswer,
    /// The provider repeats one call id across two requests.
    DuplicateToolCall,
    /// The provider requests a tool whose result is very large.
    LargeToolResult,
    /// The provider rejects the first request with a context overflow.
    ContextOverflow,
    /// The provider rejects every request with a context overflow.
    PersistentContextOverflow,
    /// The provider keeps reporting unfinished work.
    UnfinishedWork,
    /// Unfinished work, answered slowly on purpose so cancel and user steer can
    /// be injected while a turn is genuinely in flight.
    SlowUnfinishedWork,
}

/// How long the slow scripted scenarios take to answer.
pub const SLOW_RESPONSE_MILLIS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateModel {
    pub scenario: GateScenario,
    /// Catalog context window in tokens.
    pub context_window: u64,
    /// Token count at which the runtime is expected to consider compaction.
    pub compact_threshold: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GateCatalog {
    pub models: BTreeMap<String, GateModel>,
}

impl GateCatalog {
    pub fn path_in(codex_home: &Path) -> PathBuf {
        codex_home.join(GATE_CATALOG_FILE)
    }

    pub fn read(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn get(&self, model: &str) -> Option<&GateModel> {
        self.models.get(model)
    }
}

/// Catalog ids used by the bridge-mode gate. They are opaque strings: the
/// bridge routes them through the trusted model map, never by prefix.
pub const OFFICIAL_MODEL: &str = "gate-gpt-control";
pub const QWEN_MODEL: &str = "gate-qwen-tools";
pub const DEEPSEEK_MODEL: &str = "gate-deepseek-context";
pub const DEEPSEEK_OVERFLOW_MODEL: &str = "gate-deepseek-overflow";
pub const DEEPSEEK_STUCK_MODEL: &str = "gate-deepseek-stuck";
pub const QWEN_CONTINUATION_MODEL: &str = "gate-qwen-continuation";
pub const QWEN_CANCEL_MODEL: &str = "gate-qwen-cancel";
pub const QWEN_STEER_MODEL: &str = "gate-qwen-steer";
pub const GROK_MODEL: &str = "gate-grok-routing";
/// A catalog id shaped like an Official model but owned by a third party. It
/// exists so classification can be proven to read the trusted map rather than
/// the model's name.
pub const NAME_TRAP_MODEL: &str = "gpt-5.4-turbo-preview";
pub const REAL_QWEN_SMOKE_MODEL: &str = "gate-real-qwen-smoke";
pub const REAL_DEEPSEEK_SMOKE_MODEL: &str = "gate-real-deepseek-smoke";
pub const REAL_GROK_SMOKE_MODEL: &str = "gate-real-grok-smoke";

pub fn default_catalog() -> GateCatalog {
    let mut models = BTreeMap::new();
    models.insert(
        OFFICIAL_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::PlainAnswer,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    models.insert(
        QWEN_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::DuplicateToolCall,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    models.insert(
        DEEPSEEK_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::LargeToolResult,
            // Deliberately small so one large tool result crosses the
            // threshold without needing a long conversation.
            context_window: 4_000,
            compact_threshold: 2_000,
        },
    );
    // The overflow models advertise a roomy window on purpose: the runtime's
    // own estimate says the surface is fine and the provider disagrees. That
    // disagreement is the whole point of overflow recovery, and it is the case
    // a pressure-triggered prune would hide.
    models.insert(
        DEEPSEEK_OVERFLOW_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::ContextOverflow,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    models.insert(
        DEEPSEEK_STUCK_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::PersistentContextOverflow,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    models.insert(
        QWEN_CONTINUATION_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::UnfinishedWork,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    for model in [QWEN_CANCEL_MODEL, QWEN_STEER_MODEL] {
        models.insert(
            model.to_string(),
            GateModel {
                scenario: GateScenario::SlowUnfinishedWork,
                context_window: 200_000,
                compact_threshold: 160_000,
            },
        );
    }
    models.insert(
        GROK_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::PlainAnswer,
            context_window: 128_000,
            compact_threshold: 100_000,
        },
    );
    models.insert(
        NAME_TRAP_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::PlainAnswer,
            context_window: 200_000,
            compact_threshold: 160_000,
        },
    );
    for model in [
        REAL_QWEN_SMOKE_MODEL,
        REAL_DEEPSEEK_SMOKE_MODEL,
        REAL_GROK_SMOKE_MODEL,
    ] {
        models.insert(
            model.to_string(),
            GateModel {
                scenario: GateScenario::PlainAnswer,
                context_window: 128_000,
                compact_threshold: 100_000,
            },
        );
    }
    GateCatalog { models }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_round_trips_and_covers_every_gated_model() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = default_catalog();
        let path = GateCatalog::path_in(temp.path());
        catalog.write(&path).unwrap();
        assert_eq!(GateCatalog::read(&path).unwrap(), catalog);
        for model in [
            OFFICIAL_MODEL,
            QWEN_MODEL,
            DEEPSEEK_MODEL,
            DEEPSEEK_OVERFLOW_MODEL,
            DEEPSEEK_STUCK_MODEL,
            QWEN_CONTINUATION_MODEL,
            QWEN_CANCEL_MODEL,
            QWEN_STEER_MODEL,
            GROK_MODEL,
            NAME_TRAP_MODEL,
        ] {
            assert!(catalog.get(model).is_some(), "{model} is missing");
        }
    }

    #[test]
    fn the_context_recovery_models_have_a_threshold_below_their_window() {
        let catalog = default_catalog();
        for model in [
            DEEPSEEK_MODEL,
            DEEPSEEK_OVERFLOW_MODEL,
            DEEPSEEK_STUCK_MODEL,
        ] {
            let entry = catalog.get(model).unwrap();
            assert!(entry.compact_threshold < entry.context_window);
        }
    }
}
