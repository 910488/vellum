use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySuite {
    cases: Vec<ReplayCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayCase {
    id: String,
    operation: String,
    input: Value,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    expected: BTreeMap<String, Value>,
    #[serde(default)]
    expected_prefixes: BTreeMap<String, String>,
    #[serde(default)]
    absent: Vec<String>,
    #[serde(default)]
    expected_error: Option<String>,
}

pub fn run(path: &Path) -> AppResult<usize> {
    let bytes = std::fs::read(path).map_err(|error| {
        AppError::Message(format!(
            "cannot read protocol replay {}: {error}",
            path.display()
        ))
    })?;
    let suite: ReplaySuite = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Message(format!(
            "invalid protocol replay {}: {error}",
            path.display()
        ))
    })?;
    if suite.cases.is_empty() {
        return Err(AppError::Message("protocol replay has no cases".into()));
    }
    for case in &suite.cases {
        let output = match execute(case) {
            Ok(output) if case.expected_error.is_none() => output,
            Ok(_) => {
                return Err(AppError::Message(format!(
                    "protocol replay {} expected an error",
                    case.id
                )))
            }
            Err(error) => {
                let Some(expected) = &case.expected_error else {
                    return Err(error);
                };
                if !error.to_string().contains(expected) {
                    return Err(AppError::Message(format!(
                        "protocol replay {} expected error containing {expected:?}, got {error}",
                        case.id
                    )));
                }
                continue;
            }
        };
        for (pointer, expected) in &case.expected {
            let actual = output.pointer(pointer).ok_or_else(|| {
                AppError::Message(format!(
                    "protocol replay {} missing expected pointer {pointer}",
                    case.id
                ))
            })?;
            if actual != expected {
                return Err(AppError::Message(format!(
                    "protocol replay {} mismatch at {pointer}: expected {expected}, got {actual}",
                    case.id
                )));
            }
        }
        for (pointer, expected_prefix) in &case.expected_prefixes {
            let actual = output
                .pointer(pointer)
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AppError::Message(format!(
                        "protocol replay {} missing expected string pointer {pointer}",
                        case.id
                    ))
                })?;
            if !actual.starts_with(expected_prefix) {
                return Err(AppError::Message(format!(
                    "protocol replay {} mismatch at {pointer}: expected prefix {expected_prefix}, got {actual}",
                    case.id
                )));
            }
        }
        for pointer in &case.absent {
            if output.pointer(pointer).is_some() {
                return Err(AppError::Message(format!(
                    "protocol replay {} unexpectedly retained {pointer}",
                    case.id
                )));
            }
        }
    }
    Ok(suite.cases.len())
}

fn execute(case: &ReplayCase) -> AppResult<Value> {
    match case.operation.as_str() {
        "responses_to_chat" => crate::adapter::responses_to_chat(&case.input),
        "chat_to_responses" => crate::adapter::chat_response_to_responses(
            &case.input,
            case.model.as_deref().unwrap_or("replay-model"),
        ),
        "sanitize_official" => {
            let mut output = case.input.clone();
            crate::adapter::sanitize_for_official(&mut output);
            Ok(output)
        }
        "strip_cross_realm" => {
            let mut output = case.input.clone();
            crate::adapter::strip_cross_realm_fields(&mut output);
            Ok(output)
        }
        "third_party_readable" => {
            let mut output = case.input.clone();
            crate::adapter::strip_opaque_reasoning_keep_summary(&mut output);
            Ok(output)
        }
        "error_envelope" => Ok(crate::adapter::error_envelope(
            429,
            "replay-provider",
            "replay-model",
            "/responses",
            "quota",
        )),
        "prepare_grok" => prepare_request(case, crate::model::ProviderKind::GrokCli),
        "prepare_compatible" => prepare_request(case, crate::model::ProviderKind::OpenAiCompatible),
        "prepare_compatible_chat" => prepare_request_with_wire(
            case,
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::WireFormat::Chat,
        ),
        "prepare_official" => Ok(crate::adapter::prepare_openai_official_native(
            &case.input,
            case.model.as_deref().unwrap_or("gpt-5.4-mini"),
        )),
        "prepare_official_handoff" => {
            let mut portable = case.input.clone();
            crate::adapter::sanitize_for_official(&mut portable);
            Ok(crate::adapter::prepare_openai_official_native(
                &portable,
                case.model.as_deref().unwrap_or("gpt-5.4-mini"),
            ))
        }
        "zstd_rebuild" => crate::proxy::replay_zstd_rebuild(&case.input),
        "websocket_shape" => crate::proxy::replay_websocket_shape(&case.input),
        "compaction_materialization" => {
            crate::proxy::replay_compaction_materialization(&case.input)
        }
        "grok_to_official_compaction" => {
            crate::proxy::replay_grok_to_official_compaction(&case.input)
        }
        "official_canonical_compaction" => {
            crate::proxy::replay_official_canonical_compaction(&case.input)
        }
        "server_side_canonical_compaction" => {
            crate::proxy::replay_server_side_canonical_compaction(&case.input)
        }
        operation => Err(AppError::Message(format!(
            "unsupported protocol replay operation: {operation}"
        ))),
    }
}

fn prepare_request(
    case: &ReplayCase,
    provider_kind: crate::model::ProviderKind,
) -> AppResult<Value> {
    prepare_request_with_wire(case, provider_kind, crate::model::WireFormat::Responses)
}

fn prepare_request_with_wire(
    case: &ReplayCase,
    provider_kind: crate::model::ProviderKind,
    wire: crate::model::WireFormat,
) -> AppResult<Value> {
    use crate::model::{AuthKind, ModelRoute, Route};
    let upstream = case.model.as_deref().unwrap_or("replay-model");
    let route = Route {
        id: "replay".into(),
        name: "replay".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        model: upstream.into(),
        wire,
        is_current: false,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        provider_kind,
        auth_kind: AuthKind::None,
        enabled: true,
        models: vec![upstream.into()],
        selected_models: Some(vec![upstream.into()]),
        context_window: Some(128_000),
        model_capabilities: Vec::new(),
        insecure_http_policy: Default::default(),
        catalog_scope: Default::default(),
    };
    let model = ModelRoute {
        catalog_id: "vlm-replay".into(),
        display_name: "Replay".into(),
        route_id: route.id.clone(),
        upstream_model: upstream.into(),
        context_window: route.context_window,
        wire: route.wire,
        reasoning: route.reasoning,
        streaming: route.streaming,
        vision: false,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
        reasoning_effort_transport: Default::default(),
    };
    crate::adapter::prepare_upstream_request(&case.input, &route, &model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_protocol_replay_passes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals")
            .join("replays")
            .join("protocol-replay.json");
        assert!(run(&path).unwrap() >= 5);
    }

    #[test]
    fn checked_in_desktop_protocol_replay_passes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals")
            .join("replays")
            .join("desktop-protocol-replay.json");
        assert!(run(&path).unwrap() >= 8);
    }

    #[test]
    fn canonical_compaction_contract_replay_passes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals")
            .join("replays")
            .join("canonical-compaction-12.json");
        assert_eq!(run(&path).unwrap(), 12);
    }
}
