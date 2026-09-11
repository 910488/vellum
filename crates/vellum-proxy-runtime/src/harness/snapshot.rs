//! Machine-readable dump of everything the model can actually see.
//!
//! Issue #6 Phase 0. The 2026-08-02 session was hard to diagnose because there
//! was no way to compare what native GPT-5.6 Sol receives against what a
//! translated Grok route receives — the catalog, the prompt, and the adapter
//! each knew a piece of the answer and nobody held the whole picture. This
//! module produces one canonical, hashable, diffable record per route.

use super::shell::TerminalCapabilities;
use super::{HarnessProfile, HarnessToolMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// One model-visible tool, canonicalized so two snapshots can be compared
/// without JSON key-order noise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSnapshot {
    pub name: String,
    /// Original Codex tool type: `function`, `custom`, `namespace`, or a
    /// built-in kind. Kept so a diff shows *how* a tool was translated.
    pub origin: String,
    pub mutating: bool,
    pub description: String,
    pub parameters: Value,
}

impl ToolSnapshot {
    pub fn new(
        name: impl Into<String>,
        origin: impl Into<String>,
        mutating: bool,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            name: name.into(),
            origin: origin.into(),
            mutating,
            description: description.into(),
            parameters: canonicalize(&parameters),
        }
    }

    /// Schema identity only — description wording changes should not read as a
    /// contract change when diffing.
    pub fn schema_fingerprint(&self) -> String {
        hash_value(&json!({
            "name": self.name,
            "origin": self.origin,
            "mutating": self.mutating,
            "parameters": self.parameters
        }))
    }
}

/// A Codex tool that was deliberately not forwarded, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedTool {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVisibleHarnessSnapshot {
    /// Which resolved contract produced this surface.
    pub adapter_profile: HarnessProfile,
    /// The catalog entry Codex will read for this model.
    pub catalog_model_info: Value,
    /// Where the instructions came from: Codex's own bundle, or a named
    /// Vellum prompt family. Never "whatever was first in the catalog".
    pub prompt_source: String,
    pub prompt_hash: String,
    pub tool_mode: String,
    pub tools: Vec<ToolSnapshot>,
    pub unsupported_tools: Vec<UnsupportedTool>,
    pub shell: Option<TerminalCapabilities>,
    pub provider_capabilities: Value,
}

impl ModelVisibleHarnessSnapshot {
    // Each argument is a distinct axis of the model-visible surface; bundling
    // them into a parameter struct would only move the same list one level up.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile: HarnessProfile,
        catalog_model_info: Value,
        prompt_source: impl Into<String>,
        prompt: &str,
        mut tools: Vec<ToolSnapshot>,
        mut unsupported_tools: Vec<UnsupportedTool>,
        shell: Option<TerminalCapabilities>,
        provider_capabilities: Value,
    ) -> Self {
        // Sort so a snapshot hash tracks the contract, not the order Codex
        // happened to declare tools in this request.
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        unsupported_tools.sort_by(|left, right| left.name.cmp(&right.name));
        Self {
            adapter_profile: profile,
            catalog_model_info: canonicalize(&catalog_model_info),
            prompt_source: prompt_source.into(),
            prompt_hash: hash_text(prompt),
            tool_mode: match profile.tool_mode {
                HarnessToolMode::CodexNative => "codexNative",
                HarnessToolMode::TranslatedDirect => "translatedDirect",
            }
            .into(),
            tools,
            unsupported_tools,
            shell,
            provider_capabilities: canonicalize(&provider_capabilities),
        }
    }

    /// Read a native Codex catalog entry — GPT-5.6 Sol, for example — so the
    /// parity target can be diffed against a generated one.
    pub fn from_official_catalog_entry(entry: &Value) -> Self {
        let prompt = entry
            .get("base_instructions")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let capabilities = json!({
            "apply_patch_tool_type": entry.get("apply_patch_tool_type"),
            "web_search_tool_type": entry.get("web_search_tool_type"),
            "input_modalities": entry.get("input_modalities"),
            "supports_parallel_tool_calls": entry.get("supports_parallel_tool_calls"),
            "tool_mode": entry.get("tool_mode"),
            "use_responses_lite": entry.get("use_responses_lite"),
            "shell_type": entry.get("shell_type"),
            "context_window": entry.get("context_window"),
            "truncation_policy": entry.get("truncation_policy"),
            "comp_hash": entry.get("comp_hash"),
            "multi_agent_version": entry.get("multi_agent_version"),
            "supported_reasoning_levels": entry.get("supported_reasoning_levels"),
            "default_reasoning_level": entry.get("default_reasoning_level"),
            "priority": entry.get("priority")
        });
        Self::new(
            HarnessProfile::official_native(),
            entry.clone(),
            "codexOfficialBundledCatalog",
            prompt,
            // Codex builds the native tool router itself; Vellum forwards the
            // request untouched and so has no authority to enumerate it.
            Vec::new(),
            Vec::new(),
            None,
            capabilities,
        )
    }

    pub fn canonical_json(&self) -> Value {
        canonicalize(&serde_json::to_value(self).unwrap_or(Value::Null))
    }

    /// Stable identity for this whole surface. Recorded per session so a
    /// change in what the model sees is visible in the JSONL.
    pub fn hash(&self) -> String {
        hash_value(&self.canonical_json())
    }

    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    /// Human-readable differences against another surface — typically the
    /// native Sol snapshot against the generated Grok one.
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        if self.adapter_profile.kind != other.adapter_profile.kind {
            differences.push(format!(
                "profile: {} -> {}",
                self.adapter_profile.kind.as_str(),
                other.adapter_profile.kind.as_str()
            ));
        }
        if self.tool_mode != other.tool_mode {
            differences.push(format!(
                "toolMode: {} -> {}",
                self.tool_mode, other.tool_mode
            ));
        }
        if self.prompt_hash != other.prompt_hash {
            differences.push(format!(
                "prompt: {} ({}) -> {} ({})",
                &self.prompt_hash[..8.min(self.prompt_hash.len())],
                self.prompt_source,
                &other.prompt_hash[..8.min(other.prompt_hash.len())],
                other.prompt_source
            ));
        }
        for (key, left) in self.provider_capabilities.as_object().into_iter().flatten() {
            let right = other.provider_capabilities.get(key).unwrap_or(&Value::Null);
            if left != right {
                differences.push(format!("capability {key}: {left} -> {right}"));
            }
        }
        for tool in &self.tools {
            match other
                .tools
                .iter()
                .find(|candidate| candidate.name == tool.name)
            {
                None => differences.push(format!("tool removed: {}", tool.name)),
                Some(candidate) if candidate.schema_fingerprint() != tool.schema_fingerprint() => {
                    differences.push(format!("tool schema changed: {}", tool.name));
                }
                Some(_) => {}
            }
        }
        for tool in &other.tools {
            if !self
                .tools
                .iter()
                .any(|candidate| candidate.name == tool.name)
            {
                differences.push(format!("tool added: {}", tool.name));
            }
        }
        for tool in &other.unsupported_tools {
            differences.push(format!("tool unsupported: {} ({})", tool.name, tool.reason));
        }
        differences
    }
}

/// Recursively sort object keys. `serde_json` is built with `preserve_order`
/// in this crate, so insertion order would otherwise leak into every hash.
pub fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(entry) = object.get(&key) {
                    sorted.insert(key, canonicalize(entry));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn hash_value(value: &Value) -> String {
    hash_text(&canonicalize(value).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::shell::{ShellProbe, TerminalCapabilities};
    use crate::harness::{resolve_with_options, HarnessOptions, HarnessProfile};
    use crate::route::{RuntimeProviderKind, RuntimeWireFormat};

    fn grok_snapshot(tools: Vec<ToolSnapshot>) -> ModelVisibleHarnessSnapshot {
        ModelVisibleHarnessSnapshot::new(
            resolve_with_options(
                RuntimeProviderKind::GrokCli,
                RuntimeWireFormat::Responses,
                HarnessOptions::default(),
                false,
            ),
            json!({"slug": "vlm-grok", "use_responses_lite": false}),
            "solCompatibleGrok",
            "instructions",
            tools,
            vec![UnsupportedTool {
                name: "mcp".into(),
                reason: "server-defined schema".into(),
            }],
            Some(TerminalCapabilities::from_probe(&ShellProbe::default())),
            json!({"tool_mode": Value::Null, "use_responses_lite": false}),
        )
    }

    #[test]
    fn canonicalization_is_key_order_independent() {
        let left = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let right = json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(canonicalize(&left), canonicalize(&right));
        assert_eq!(hash_value(&left), hash_value(&right));
    }

    #[test]
    fn snapshot_hash_tracks_the_contract_not_the_declaration_order() {
        let first = grok_snapshot(vec![
            ToolSnapshot::new("shell", "shell", true, "run", json!({"type": "object"})),
            ToolSnapshot::new(
                "apply_patch",
                "custom",
                true,
                "patch",
                json!({"type": "object"}),
            ),
        ]);
        let reordered = grok_snapshot(vec![
            ToolSnapshot::new(
                "apply_patch",
                "custom",
                true,
                "patch",
                json!({"type": "object"}),
            ),
            ToolSnapshot::new("shell", "shell", true, "run", json!({"type": "object"})),
        ]);
        assert_eq!(first.hash(), reordered.hash());
        assert_eq!(first.tool_names(), vec!["apply_patch", "shell"]);
    }

    #[test]
    fn schema_changes_are_visible_but_wording_changes_are_not() {
        let base = grok_snapshot(vec![ToolSnapshot::new(
            "apply_patch",
            "custom",
            true,
            "patch",
            json!({"type": "object", "required": ["patch"]}),
        )]);
        let reworded = grok_snapshot(vec![ToolSnapshot::new(
            "apply_patch",
            "custom",
            true,
            "a different description",
            json!({"required": ["patch"], "type": "object"}),
        )]);
        assert!(base
            .diff(&reworded)
            .iter()
            .all(|entry| !entry.starts_with("tool schema changed")));

        let loosened = grok_snapshot(vec![ToolSnapshot::new(
            "apply_patch",
            "custom",
            true,
            "patch",
            json!({"type": "object", "additionalProperties": true}),
        )]);
        assert!(base
            .diff(&loosened)
            .contains(&"tool schema changed: apply_patch".to_string()));
    }

    #[test]
    fn native_sol_snapshot_diffs_against_the_generated_grok_surface() {
        let sol = ModelVisibleHarnessSnapshot::from_official_catalog_entry(&json!({
            "slug": "gpt-5.6-sol",
            "base_instructions": "sol instructions",
            "apply_patch_tool_type": "freeform",
            "tool_mode": "code_mode_only",
            "use_responses_lite": true,
            "supports_parallel_tool_calls": true,
            "multi_agent_version": "v2",
            "priority": 1
        }));
        assert_eq!(sol.prompt_source, "codexOfficialBundledCatalog");
        assert_eq!(sol.adapter_profile, HarnessProfile::official_native());

        let differences = sol.diff(&grok_snapshot(vec![ToolSnapshot::new(
            "apply_patch",
            "custom",
            true,
            "patch",
            json!({"type": "object"}),
        )]));
        assert!(differences
            .iter()
            .any(|entry| entry.starts_with("profile: codexOfficialNative")));
        assert!(differences
            .iter()
            .any(|entry| entry.contains("capability tool_mode")));
        assert!(differences
            .iter()
            .any(|entry| entry.contains("capability use_responses_lite")));
        assert!(differences
            .iter()
            .any(|entry| entry == "tool added: apply_patch"));
        assert!(differences
            .iter()
            .any(|entry| entry.starts_with("tool unsupported: mcp")));
        assert!(differences
            .iter()
            .any(|entry| entry.starts_with("prompt: ")));
    }
}
