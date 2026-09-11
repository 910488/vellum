//! Pure protocol-replay helpers retained by `vellum-eval` after M11.
//!
//! These functions exercise serialization and canonical compaction contracts;
//! they deliberately contain no HTTP provider dispatch path.

use std::collections::HashSet;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

pub(crate) fn replay_zstd_rebuild(input: &Value) -> AppResult<Value> {
    let encoded = serde_json::to_vec(input)
        .map_err(|error| AppError::Message(format!("encode replay JSON: {error}")))?;
    let compressed = zstd::stream::encode_all(encoded.as_slice(), 1)
        .map_err(|error| AppError::Message(format!("compress replay JSON: {error}")))?;
    let decoded = zstd::stream::decode_all(compressed.as_slice())
        .map_err(|error| AppError::Message(format!("decode replay JSON: {error}")))?;
    Ok(json!({
        "body": serde_json::from_slice::<Value>(&decoded)
            .map_err(|error| AppError::Message(format!("parse replay JSON: {error}")))?,
        "contentEncodingForwarded": false,
        "contentLength": Value::Null,
        "contentType": "application/json"
    }))
}

pub(crate) fn replay_websocket_shape(input: &Value) -> AppResult<Value> {
    let text = input.to_string();
    let from_text: Value = serde_json::from_str(&text)
        .map_err(|error| AppError::Message(format!("text WebSocket replay: {error}")))?;
    let from_binary: Value = serde_json::from_slice(text.as_bytes())
        .map_err(|error| AppError::Message(format!("binary WebSocket replay: {error}")))?;
    if from_text != from_binary {
        return Err(AppError::Message(
            "text and binary WebSocket request shapes diverged".into(),
        ));
    }
    Ok(from_text)
}

pub(crate) fn replay_compaction_materialization(input: &Value) -> AppResult<Value> {
    let summary = input
        .get("summary")
        .cloned()
        .unwrap_or_else(|| json!({"type": "message", "role": "user", "content": "summary"}));
    Ok(json!({
        "official": [summary.clone()],
        "thirdParty": [summary]
    }))
}

pub(crate) fn replay_grok_to_official_compaction(input: &Value) -> AppResult<Value> {
    let id = input
        .get("compactionId")
        .and_then(Value::as_str)
        .unwrap_or("cmp_desktop_grok_to_official");
    Ok(json!({
        "model": "gpt-5.6-sol",
        "input": [
            {"type": "compaction", "id": id, "encrypted_content": "opaque-official-state"},
            {"type": "message", "role": "user", "content": "continue"}
        ]
    }))
}

pub(crate) fn replay_official_canonical_compaction(input: &Value) -> AppResult<Value> {
    let source = input
        .get("source")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| {
            vec![json!({"type": "message", "role": "user", "content": "remember 27"})]
        });
    let canonical = input
        .get("canonical")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| AppError::Message("canonical replay requires canonical items".into()))?;
    let compact_id = canonical
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .and_then(|item| item.get("id").and_then(Value::as_str))
        .ok_or_else(|| {
            AppError::Message("canonical replay requires a compaction item id".into())
        })?;
    let portable = input.get("portable").and_then(Value::as_array).cloned();
    let third_party = input.get("target").and_then(Value::as_str) == Some("third_party");
    let replacement = if third_party {
        portable.clone().ok_or_else(|| {
            AppError::Message("no validated portable handoff window for compaction".into())
        })?
    } else {
        canonical.clone()
    };
    let resume = input
        .get("resume")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| {
            vec![
                json!({"type": "compaction", "id": compact_id}),
                json!({"type": "message", "role": "user", "content": "continue"}),
            ]
        });
    let resume = materialize(&resume, compact_id, &replacement);
    let canonical_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    );
    Ok(json!({
        "journal": {
            "schemaVersion": 3,
            "kind": "Official",
            "canonicalHash": canonical_hash,
            "canonicalItems": canonical,
            "sourceItems": source,
            "portableAvailable": portable.is_some()
        },
        "resume": resume
    }))
}

pub(crate) fn replay_server_side_canonical_compaction(input: &Value) -> AppResult<Value> {
    let output = input
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| AppError::Message("server-side replay requires output items".into()))?;
    let index = output
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .map(|(index, _)| index)
        .ok_or_else(|| {
            AppError::Message("server-side replay output contained no compaction item".into())
        })?;
    Ok(json!({
        "canonicalItems": output[index..],
        "kind": "Official",
        "trigger": "official_context_management"
    }))
}

fn materialize(input: &[Value], compact_id: &str, replacement: &[Value]) -> Vec<Value> {
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for item in input {
        let is_reference = item.get("type").and_then(Value::as_str) == Some("compaction")
            && item.get("id").and_then(Value::as_str) == Some(compact_id);
        let candidates: &[Value] = if is_reference {
            replacement
        } else {
            std::slice::from_ref(item)
        };
        for candidate in candidates {
            let key = candidate
                .get("id")
                .and_then(Value::as_str)
                .map(|id| format!("id:{id}"))
                .unwrap_or_else(|| candidate.to_string());
            if seen.insert(key) {
                output.push(candidate.clone());
            }
        }
    }
    output
}
