use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{ModelRoute, ProviderKind, Route};

pub const MODEL_PROVIDER_MAP_SCHEMA_VERSION: u32 = 1;

/// Trusted control-plane identity for one exact Codex catalog slug.
///
/// `provider_id` is the logical Vellum Provider used for routing and durable
/// bindings. `child_provider_id` is the Codex provider table the chosen child
/// must load -- always one Vellum wrote into the Codex config, never a
/// built-in, so every turn reaches Vellum's proxy and carries the account the
/// user selected. The map is generated from Vellum's catalog; the bridge never
/// guesses from a model-name prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderRoute {
    pub provider_id: String,
    pub child_provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedModelProviderMap {
    pub schema_version: u32,
    pub models: BTreeMap<String, ModelProviderRoute>,
}

impl Default for TrustedModelProviderMap {
    fn default() -> Self {
        Self {
            schema_version: MODEL_PROVIDER_MAP_SCHEMA_VERSION,
            models: BTreeMap::new(),
        }
    }
}

impl TrustedModelProviderMap {
    pub fn from_catalog(routes: &[Route], models: &[ModelRoute]) -> Self {
        let route_kinds = routes
            .iter()
            .map(|route| (route.id.as_str(), route.provider_kind))
            .collect::<BTreeMap<_, _>>();
        let models = models
            .iter()
            .filter_map(|model| {
                let kind = route_kinds.get(model.route_id.as_str())?;
                let (provider_id, child_provider_id) = if *kind == ProviderKind::Official {
                    // Official binds to Vellum's second provider table, not to
                    // Codex's built-in `openai`. The built-in table talks
                    // straight to ChatGPT with the Codex install's own
                    // `auth.json`, which is a different account store from the
                    // one Vellum's account switch writes -- so an Official turn
                    // ran as whoever Codex Desktop was logged in as, whatever
                    // the user had selected, and Vellum never saw the turn.
                    (
                        "openai-official".to_string(),
                        crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME.to_string(),
                    )
                } else {
                    (
                        model.route_id.clone(),
                        crate::codex::VELLUM_PROVIDER_NAME.to_string(),
                    )
                };
                Some((
                    model.catalog_id.clone(),
                    ModelProviderRoute {
                        provider_id,
                        child_provider_id,
                    },
                ))
            })
            .collect();
        Self {
            schema_version: MODEL_PROVIDER_MAP_SCHEMA_VERSION,
            models,
        }
    }

    pub fn read(path: &Path) -> Result<Self, ModelProviderMapError> {
        let map = serde_json::from_slice::<Self>(&std::fs::read(path)?)?;
        map.validate()?;
        Ok(map)
    }

    pub fn write(&self, path: &Path) -> Result<(), ModelProviderMapError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn resolve(&self, catalog_id: &str) -> Option<&ModelProviderRoute> {
        self.models.get(catalog_id)
    }

    fn validate(&self) -> Result<(), ModelProviderMapError> {
        if self.schema_version != MODEL_PROVIDER_MAP_SCHEMA_VERSION {
            return Err(ModelProviderMapError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        for (model, route) in &self.models {
            if model.trim().is_empty()
                || route.provider_id.trim().is_empty()
                || route.child_provider_id.trim().is_empty()
            {
                return Err(ModelProviderMapError::EmptyIdentity(model.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelProviderMapError {
    #[error("unsupported model provider map schema {0}")]
    UnsupportedSchema(u32),
    #[error("model provider map contains an empty identity for {0}")]
    EmptyIdentity(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(id: &str, kind: &str) -> Route {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "baseUrl": "https://example.invalid/v1",
            "model": "m",
            "wire": "responses",
            "isCurrent": false,
            "serverSideResume": true,
            "streaming": true,
            "reasoning": true,
            "providerKind": kind,
        }))
        .expect("route fixture")
    }

    fn model(catalog_id: &str, route_id: &str) -> ModelRoute {
        serde_json::from_value(json!({
            "catalogId": catalog_id,
            "displayName": catalog_id,
            "routeId": route_id,
            "upstreamModel": catalog_id,
            "contextWindow": 400000,
            "wire": "responses",
            "reasoning": true,
            "streaming": true,
        }))
        .expect("model fixture")
    }

    /// The regression this map caused once: binding Official to Codex's
    /// built-in `openai` table sent every Official turn straight to ChatGPT
    /// under the Codex install's own `auth.json`. Vellum's account switch
    /// writes a different store, so the selected account was silently ignored
    /// and the turn never reached Vellum's proxy, usage, or quota.
    #[test]
    fn official_models_route_through_vellum_not_the_built_in_openai_table() {
        let map = TrustedModelProviderMap::from_catalog(
            &[
                route("openai-official", "official"),
                route("qwen", "openAiCompatible"),
            ],
            &[
                model("gpt-5.6-luna", "openai-official"),
                model("qwen3", "qwen"),
            ],
        );

        let official = map
            .resolve("gpt-5.6-luna")
            .expect("official model is mapped");
        assert_eq!(official.provider_id, "openai-official");
        assert_eq!(
            official.child_provider_id,
            crate::codex::VELLUM_OFFICIAL_PROVIDER_NAME
        );
        assert_ne!(
            official.child_provider_id, "openai",
            "the built-in table carries the Codex install's own login, not the selected account"
        );

        let third_party = map.resolve("qwen3").expect("third-party model is mapped");
        assert_eq!(third_party.provider_id, "qwen");
        assert_eq!(
            third_party.child_provider_id,
            crate::codex::VELLUM_PROVIDER_NAME,
            "third-party models keep the non-OpenAI table and its local compaction"
        );
        assert_ne!(
            third_party.child_provider_id, official.child_provider_id,
            "the two kinds of route need two tables to answer is_openai() differently"
        );
    }
}
