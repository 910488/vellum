//! Pure delegation contracts shared by catalog generation and request
//! translation (plan §23.1).
//!
//! The adapter advertises a `multi_agent` namespace only when a delegation
//! runtime has actually been proven, and `ultra` (maximum reasoning *with
//! automatic task delegation*) is filtered out until `delegation_verified`.
//! Host-specific wiring — whether a runtime is actually connected — is passed
//! in by each host's thin environment adapter, never decided here.

use serde_json::{json, Value};

/// The namespace all Vellum delegation tools live under.
pub const NAMESPACE: &str = "multi_agent";

/// Hard bounds the delegation runtime enforces.
pub const MAX_CHILDREN: usize = 8;
pub const MAX_CONCURRENT_CHILDREN: usize = 4;
pub const MAX_DEPTH: usize = 1;

/// The delegation tool surface, or `None` when the runtime is not verified.
///
/// Nothing at all is advertised rather than a disabled-looking tool, because a
/// tool the model can see is a tool the model will try.
pub fn delegation_namespace(verified: bool) -> Option<Value> {
    if !verified {
        return None;
    }
    Some(json!({
        "type": "namespace",
        "name": NAMESPACE,
        "description": "Delegate a scoped sub-task to a child agent and collect its result.",
        "tools": [
            {
                "type": "function",
                "name": "spawn",
                "description": "Run a scoped sub-task on a child agent and return its result. The child has no tools: give it a question to answer or material to analyse, not work to perform. Name the model explicitly — it is never substituted, and only models on this route are available. This call completes the child before it returns.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "model": {"type": "string"},
                        "reasoning_effort": {"type": "string"},
                        "instructions": {"type": "string"}
                    },
                    "required": ["model", "instructions"],
                    "additionalProperties": false
                }
            }
        ]
    }))
}

/// Reasoning efforts a route may offer.
///
/// `ultra` is maximum reasoning *with automatic task delegation*. Offering it
/// without a delegation runtime that has been proven would promise exactly the
/// behaviour this module has not yet earned, so it is filtered out until
/// `delegation_verified`.
pub fn reasoning_efforts_for(supported: &[String], delegation_verified: bool) -> Vec<String> {
    supported
        .iter()
        .filter(|effort| delegation_verified || !is_delegating_effort(effort))
        .cloned()
        .collect()
}

/// Efforts whose contract includes automatic task delegation.
pub fn is_delegating_effort(effort: &str) -> bool {
    effort.eq_ignore_ascii_case("ultra")
}

/// Relative strength of the reasoning levels Codex and Grok actually use.
///
/// Needed because a provider's `supported_reasoning_levels` array is in no
/// guaranteed order — picking "the last one" as the strongest is a guess that
/// silently changes a model's default when the provider happens to list levels
/// descending.
fn effort_rank(effort: &str) -> Option<u8> {
    match effort.to_ascii_lowercase().as_str() {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "ultra" => Some(6),
        _ => None,
    }
}

/// The strongest level in `permitted` whose strength is actually known.
pub fn strongest_permitted_effort(permitted: &[String]) -> Option<String> {
    permitted
        .iter()
        .filter_map(|effort| effort_rank(effort).map(|rank| (rank, effort)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, effort)| effort.clone())
}

/// Apply the gate to a model's advertised reasoning levels.
///
/// The only default this rewrites is one that had to be withdrawn. Everything
/// else is left exactly as the provider reported it, including *absence*:
/// returning a synthesized default where the provider gave none would override
/// the catalog's own first-level fallback and silently change which effort
/// third-party models run at. That regression is why this is spelled out one
/// case at a time rather than written as a `filter().or_else()` chain.
pub fn gate_reasoning_levels(
    supported: &[String],
    default: Option<String>,
    delegation_verified: bool,
) -> (Vec<String>, Option<String>) {
    let permitted = reasoning_efforts_for(supported, delegation_verified);
    let resolved = match default {
        // Still permitted: keep the provider's own choice untouched.
        Some(effort)
            if permitted
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&effort)) =>
        {
            Some(effort)
        }
        // Withdrawn because it delegates: fall back to the strongest level
        // that remains, by defined rank rather than array position.
        Some(effort) if is_delegating_effort(&effort) => strongest_permitted_effort(&permitted),
        // No default, or one that was never in the supported list. Report
        // nothing and let the catalog apply the fallback it always has.
        Some(_) | None => None,
    };
    (permitted, resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_namespace_is_advertised_only_when_verified() {
        assert!(delegation_namespace(false).is_none());
        let namespace = delegation_namespace(true).unwrap();
        assert_eq!(namespace["name"], NAMESPACE);
        assert_eq!(namespace["tools"][0]["name"], "spawn");
    }

    #[test]
    fn ultra_is_a_delegating_effort_and_is_filtered_until_verified() {
        assert!(is_delegating_effort("ultra"));
        assert!(!is_delegating_effort("high"));
        let supported = vec![
            "none".to_string(),
            "high".to_string(),
            "ultra".to_string(),
            "xhigh".to_string(),
        ];
        assert_eq!(
            reasoning_efforts_for(&supported, false),
            vec!["none", "high", "xhigh"]
        );
        assert_eq!(
            reasoning_efforts_for(&supported, true),
            vec!["none", "high", "ultra", "xhigh"]
        );
    }

    #[test]
    fn strongest_permitted_effort_ignores_unknown_levels() {
        let permitted = vec![
            "medium".to_string(),
            "mega".to_string(),
            "xhigh".to_string(),
        ];
        assert_eq!(
            strongest_permitted_effort(&permitted).as_deref(),
            Some("xhigh")
        );
        assert_eq!(strongest_permitted_effort(&["mega".to_string()]), None);
    }

    #[test]
    fn gate_reasoning_levels_keeps_defaults_and_withdraws_ultra() {
        let supported = vec!["none".to_string(), "high".to_string(), "ultra".to_string()];
        assert_eq!(
            gate_reasoning_levels(&supported, Some("high".to_string()), false),
            (
                vec!["none".to_string(), "high".to_string()],
                Some("high".to_string())
            )
        );
        let (permitted, resolved) =
            gate_reasoning_levels(&supported, Some("ultra".to_string()), false);
        assert_eq!(permitted, vec!["none".to_string(), "high".to_string()]);
        assert_eq!(resolved, Some("high".to_string()));
        // Verified delegation keeps ultra and its default untouched.
        assert_eq!(
            gate_reasoning_levels(&supported, Some("ultra".to_string()), true),
            (
                vec!["none".to_string(), "high".to_string(), "ultra".to_string()],
                Some("ultra".to_string())
            )
        );
        // An unknown default is reported as absent, not synthesised.
        assert_eq!(
            gate_reasoning_levels(&supported, Some("mega".to_string()), false),
            (vec!["none".to_string(), "high".to_string()], None)
        );
    }
}
