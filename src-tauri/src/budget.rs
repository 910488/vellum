//! 上下文視窗預算解析。
//!
//! 解析順序（doc/04）：
//!   1. 使用者手動覆寫
//!   2. 供應商自帶的模型快取
//!   3. Codex 型錄 context_window × effective_percent
//!   4. 保底常數
//!
//! 回傳時一定帶著 `source`，UI 才能告訴使用者這個數字從哪來 ——
//! 給一個沒有出處的常數，使用者沒辦法判斷它對不對。

use crate::model::{BudgetSource, ContextBudget};

/// 128k × 95%。只有前三層都取不到才會用到。
pub const FALLBACK_TOKENS: u64 = 121_600;
pub const DEFAULT_EFFECTIVE_PERCENT: u32 = 95;
pub const DEFAULT_COMPACT_THRESHOLD: u32 = 80;

pub fn grok_model_cache_window(model: &str) -> Option<u64> {
    let path = crate::grok_auth::grok_home().join("models_cache.json");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    find_model_window(&value, model)
}

/// Read the exact reasoning effort identifiers advertised by the installed
/// Grok CLI. This is provider-owned metadata, so no Vellum level translation
/// is applied.
pub fn grok_model_cache_efforts(model: &str) -> (Vec<String>, Option<String>) {
    let path = crate::grok_auth::grok_home().join("models_cache.json");
    let Ok(bytes) = std::fs::read(path) else {
        return (Vec::new(), None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (Vec::new(), None);
    };
    find_model_efforts(&value, model).unwrap_or_default()
}

fn find_model_efforts(
    value: &serde_json::Value,
    model: &str,
) -> Option<(Vec<String>, Option<String>)> {
    match value {
        serde_json::Value::Object(object) => {
            let matches_model = object
                .get("id")
                .or_else(|| object.get("model"))
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(model));
            if matches_model {
                let info = object.get("info").unwrap_or(value);
                let mut levels = Vec::new();
                if let Some(entries) = info.get("reasoning_efforts").and_then(|v| v.as_array()) {
                    for entry in entries {
                        let level = entry
                            .get("value")
                            .or_else(|| entry.get("id"))
                            .and_then(|v| v.as_str());
                        if let Some(level) = level.filter(|level| !level.trim().is_empty()) {
                            if !levels.iter().any(|known| known == level) {
                                levels.push(level.to_string());
                            }
                        }
                    }
                }
                let default = info
                    .get("reasoning_effort")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .or_else(|| {
                        info.get("reasoning_efforts")
                            .and_then(|v| v.as_array())
                            .and_then(|entries| {
                                entries.iter().find_map(|entry| {
                                    entry
                                        .get("default")
                                        .and_then(|v| v.as_bool())
                                        .filter(|is_default| *is_default)
                                        .and_then(|_| {
                                            entry
                                                .get("value")
                                                .or_else(|| entry.get("id"))
                                                .and_then(|v| v.as_str())
                                        })
                                        .map(str::to_owned)
                                })
                            })
                    })
                    .filter(|candidate| levels.iter().any(|level| level == candidate));
                if !levels.is_empty() {
                    return Some((levels, default));
                }
            }
            object
                .values()
                .find_map(|child| find_model_efforts(child, model))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| find_model_efforts(child, model)),
        _ => None,
    }
}

fn find_model_window(value: &serde_json::Value, model: &str) -> Option<u64> {
    match value {
        serde_json::Value::Object(object) => {
            let matches_model = object
                .get("id")
                .or_else(|| object.get("model"))
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(model));
            if matches_model {
                for key in ["context_window", "contextWindow", "max_context_length"] {
                    if let Some(window) = object
                        .get(key)
                        .and_then(serde_json::Value::as_u64)
                        .filter(|window| *window > 0)
                    {
                        return Some(window);
                    }
                }
            }
            object
                .values()
                .find_map(|child| find_model_window(child, model))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| find_model_window(child, model)),
        _ => None,
    }
}

/// 各層來源提供的原始視窗大小。`None` 代表該層沒有資料。
#[derive(Debug, Default, Clone, Copy)]
pub struct BudgetInputs {
    pub override_tokens: Option<u64>,
    pub model_cache: Option<u64>,
    pub catalog: Option<u64>,
    pub effective_percent: Option<u32>,
}

pub fn resolve(route_id: &str, model: &str, inputs: BudgetInputs) -> ContextBudget {
    let percent = inputs
        .effective_percent
        .filter(|p| *p > 0 && *p <= 100)
        .unwrap_or(DEFAULT_EFFECTIVE_PERCENT);

    // 手動覆寫是「有效視窗」本身，不再乘 percent —— 使用者填的就是他要的數字。
    if let Some(tokens) = inputs.override_tokens.filter(|t| *t > 0) {
        return ContextBudget {
            route_id: route_id.to_string(),
            catalog_id: route_id.to_string(),
            model: model.to_string(),
            source: BudgetSource::Override,
            context_window: tokens,
            effective_percent: 100,
            effective_window: tokens,
            override_tokens: Some(tokens),
            compact_threshold_percent: DEFAULT_COMPACT_THRESHOLD,
        };
    }

    let (source, window) = match (
        inputs.model_cache.filter(|w| *w > 0),
        inputs.catalog.filter(|w| *w > 0),
    ) {
        (Some(w), _) => (BudgetSource::ModelCache, w),
        (None, Some(w)) => (BudgetSource::Catalog, w),
        (None, None) => (BudgetSource::Fallback, FALLBACK_TOKENS),
    };

    // 保底值本身已經是「有效」視窗，不再打折。
    let effective = if matches!(source, BudgetSource::Fallback) {
        window
    } else {
        window.saturating_mul(u64::from(percent)) / 100
    };

    ContextBudget {
        route_id: route_id.to_string(),
        catalog_id: route_id.to_string(),
        model: model.to_string(),
        source,
        context_window: window,
        effective_percent: if matches!(source, BudgetSource::Fallback) {
            100
        } else {
            percent
        },
        effective_window: effective.max(1),
        override_tokens: None,
        compact_threshold_percent: DEFAULT_COMPACT_THRESHOLD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_every_other_source() {
        let b = resolve(
            "r",
            "m",
            BudgetInputs {
                override_tokens: Some(50_000),
                model_cache: Some(500_000),
                catalog: Some(272_000),
                effective_percent: Some(95),
            },
        );
        assert!(matches!(b.source, BudgetSource::Override));
        assert_eq!(b.effective_window, 50_000);
        assert_eq!(b.override_tokens, Some(50_000));
    }

    #[test]
    fn model_cache_beats_catalog() {
        let b = resolve(
            "r",
            "grok-4.5",
            BudgetInputs {
                model_cache: Some(500_000),
                catalog: Some(272_000),
                effective_percent: Some(95),
                ..Default::default()
            },
        );
        assert!(matches!(b.source, BudgetSource::ModelCache));
        assert_eq!(b.context_window, 500_000);
        assert_eq!(b.effective_window, 475_000);
    }

    #[test]
    fn catalog_applies_effective_percent() {
        let b = resolve(
            "r",
            "gpt-5.6-sol",
            BudgetInputs {
                catalog: Some(272_000),
                effective_percent: Some(95),
                ..Default::default()
            },
        );
        assert!(matches!(b.source, BudgetSource::Catalog));
        assert_eq!(b.effective_window, 258_400);
    }

    #[test]
    fn falls_back_when_nothing_is_known() {
        let b = resolve("r", "mystery", BudgetInputs::default());
        assert!(matches!(b.source, BudgetSource::Fallback));
        assert_eq!(b.effective_window, FALLBACK_TOKENS);
        // 保底值不再打折，否則會變成 121600 × 95%。
        assert_eq!(b.effective_percent, 100);
    }

    #[test]
    fn zero_and_absurd_percent_are_ignored() {
        for percent in [Some(0), Some(101), None] {
            let b = resolve(
                "r",
                "m",
                BudgetInputs {
                    model_cache: Some(200_000),
                    effective_percent: percent,
                    ..Default::default()
                },
            );
            assert_eq!(
                b.effective_percent, DEFAULT_EFFECTIVE_PERCENT,
                "{percent:?}"
            );
            assert_eq!(b.effective_window, 190_000);
        }
    }

    #[test]
    fn zero_override_is_treated_as_auto() {
        let b = resolve(
            "r",
            "m",
            BudgetInputs {
                override_tokens: Some(0),
                model_cache: Some(300_000),
                effective_percent: Some(95),
                ..Default::default()
            },
        );
        assert!(matches!(b.source, BudgetSource::ModelCache));
        assert_eq!(b.override_tokens, None);
    }

    #[test]
    fn effective_window_is_never_zero() {
        let b = resolve(
            "r",
            "m",
            BudgetInputs {
                model_cache: Some(1),
                effective_percent: Some(1),
                ..Default::default()
            },
        );
        assert!(b.effective_window >= 1);
    }

    #[test]
    fn grok_cache_efforts_keep_cli_values_and_default() {
        let value = serde_json::json!({
            "models": {
                "grok-4.5": {
                    "id": "grok-4.5",
                    "info": {
                        "reasoning_efforts": [
                            {"id": "high", "value": "high", "default": true},
                            {"id": "medium", "value": "medium", "default": false}
                        ],
                        "reasoning_effort": "high"
                    }
                }
            }
        });
        let (levels, default) = find_model_efforts(&value, "GROK-4.5").unwrap();
        assert_eq!(levels, vec!["high", "medium"]);
        assert_eq!(default.as_deref(), Some("high"));
    }
}
