//! Codex Turn and Subagent Identity Metadata wire parser, normalizer, conflict detector,
//! and capability inference engine.
//!
//! Protocol policy: **latest-first, stable-qualified, capability-based backward compatibility**.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::request::RuntimeEndpoint;

pub const X_CODEX_TURN_METADATA: &str = "x-codex-turn-metadata";
pub const X_CODEX_PARENT_THREAD_ID: &str = "x-codex-parent-thread-id";
pub const X_OPENAI_SUBAGENT: &str = "x-openai-subagent";
pub const X_CODEX_WINDOW_ID: &str = "x-codex-window-id";

pub const MAX_CODEX_TURN_METADATA_BYTES: usize = 16 * 1024;
pub const MAX_CODEX_ID_BYTES: usize = 256;
pub const MAX_UNKNOWN_METADATA_KEYS: usize = 32;

/// Diagnostic source name for malformed turn metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSourceName {
    CanonicalBody,
    TurnMetadataHeader,
    CompatibilityHeaders,
}

/// Errors occurring during Codex metadata extraction or ID parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodexMetadataError {
    #[error("Codex metadata field {field} is empty")]
    EmptyId { field: &'static str },

    #[error("Codex metadata field {field} is invalid")]
    InvalidId { field: &'static str },

    #[error("Codex metadata field {field} exceeds size limit: {len} > 256")]
    IdTooLong { field: &'static str, len: usize },

    #[error("Codex turn metadata is malformed ({source_name:?}): {reason}")]
    MalformedTurnMetadata {
        source_name: MetadataSourceName,
        reason: String,
    },

    #[error("Codex turn metadata exceeds size limit")]
    MetadataTooLarge,
}

/// Bounded opaque identifier preserving wire format across protocol changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodexOpaqueId(String);

impl CodexOpaqueId {
    pub fn new(field: &'static str, value: &str) -> Result<Self, CodexMetadataError> {
        parse_codex_opaque_id(field, value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CodexOpaqueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for CodexOpaqueId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for CodexOpaqueId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Parse and validate an opaque Codex identity field.
pub fn parse_codex_opaque_id(
    field: &'static str,
    value: &str,
) -> Result<CodexOpaqueId, CodexMetadataError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CodexMetadataError::EmptyId { field });
    }
    if trimmed.len() > MAX_CODEX_ID_BYTES {
        return Err(CodexMetadataError::IdTooLong {
            field,
            len: trimmed.len(),
        });
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(CodexMetadataError::InvalidId { field });
    }
    Ok(CodexOpaqueId(trimmed.to_string()))
}

/// Source from which turn identity was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexIdentitySource {
    #[default]
    None,
    CanonicalClientMetadata,
    TurnMetadataHeader,
    FlatCompatibilityHeaders,
    MergedStructuredAndCompatibility,
    LegacyHeuristic,
}

/// Trust level of the extracted Codex identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexIdentityTrust {
    #[default]
    None,
    Partial,
    Structured,
    Exact,
    Conflict,
}

/// Normalized Codex turn identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTurnIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<CodexOpaqueId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_thread_id: Option<CodexOpaqueId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<CodexOpaqueId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_id: Option<CodexOpaqueId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_header: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<serde_json::Value>,

    #[serde(default)]
    pub source: CodexIdentitySource,
    #[serde(default)]
    pub trust: CodexIdentityTrust,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_fingerprint: Option<String>,
}

/// Tolerant wire format for structured Codex metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct CodexTurnMetadataWire {
    pub installation_id: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub agent_name: Option<String>,
    pub turn_id: Option<String>,
    pub root_turn_id: Option<String>,

    pub parent_thread_id: Option<String>,
    pub parent_turn_id: Option<String>,
    pub forked_from_thread_id: Option<String>,

    pub window_id: Option<String>,
    pub context_window_id: Option<String>,

    pub request_kind: Option<String>,
    pub subagent_kind: Option<String>,
    pub thread_source: Option<Value>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Scope of the HTTP headers presented with metadata extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CodexMetadataHeaderScope {
    /// The headers belong strictly to the same HTTP request as the body.
    /// In this scope, mismatches between canonical body metadata and structured headers are critical conflicts.
    #[default]
    SameRequest,
    /// The headers belong to a persistent connection handshake (e.g. WebSocket upgrade)
    /// while the body belongs to an individual per-turn frame.
    /// In this scope, per-turn canonical body metadata is authoritative and outranks stale connection handshake headers.
    ConnectionProjection,
}

/// Container for metadata extraction sources.
pub struct CodexMetadataSources<'a> {
    pub headers: &'a HeaderMap,
    pub body: &'a Value,
    pub endpoint: RuntimeEndpoint,
    pub header_scope: CodexMetadataHeaderScope,
}

impl<'a> CodexMetadataSources<'a> {
    pub fn new(headers: &'a HeaderMap, body: &'a Value, endpoint: RuntimeEndpoint) -> Self {
        Self {
            headers,
            body,
            endpoint,
            header_scope: CodexMetadataHeaderScope::SameRequest,
        }
    }

    pub fn with_scope(mut self, scope: CodexMetadataHeaderScope) -> Self {
        self.header_scope = scope;
        self
    }
}

/// Flat compatibility headers and client_metadata fields projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCompatibilityProjection {
    pub session_id: Option<CodexOpaqueId>,
    pub thread_id: Option<CodexOpaqueId>,
    pub turn_id: Option<CodexOpaqueId>,
    pub parent_thread_id: Option<CodexOpaqueId>,
    pub parent_turn_id: Option<CodexOpaqueId>,
    pub root_turn_id: Option<CodexOpaqueId>,
    pub subagent_header: Option<String>,
    pub window_id: Option<CodexOpaqueId>,
}

/// Mismatched critical fields that lead to conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexMetadataConflict {
    pub fields: Vec<&'static str>,
}

/// Extract normalized `CodexTurnIdentity` from incoming headers and body.
pub fn extract_codex_turn_identity(
    sources: CodexMetadataSources<'_>,
) -> Result<CodexTurnIdentity, CodexMetadataError> {
    let canonical_body = parse_canonical_client_metadata(sources.body)?;

    let structured_header_wire = parse_turn_metadata_header(sources.headers)?;
    let structured_header_identity = if let Some(wire) = structured_header_wire {
        Some(normalize_wire_metadata(
            wire,
            CodexIdentitySource::TurnMetadataHeader,
        )?)
    } else {
        None
    };

    let flat_headers = parse_flat_header_compatibility(sources.headers)?;
    let flat_body = parse_flat_body_compatibility(sources.body)?;

    let merged = merge_identity_sources(
        canonical_body,
        structured_header_identity,
        flat_body,
        flat_headers,
        sources.header_scope,
    );

    Ok(merged)
}

/// Parse canonical metadata from `client_metadata["x-codex-turn-metadata"]` if present.
pub fn parse_canonical_client_metadata(
    body: &Value,
) -> Result<Option<CodexTurnIdentity>, CodexMetadataError> {
    let Some(client_metadata) = body.get("client_metadata").and_then(Value::as_object) else {
        return Ok(None);
    };

    let Some(wire_val) = client_metadata.get(X_CODEX_TURN_METADATA) else {
        return Ok(None);
    };

    let wire: CodexTurnMetadataWire = match wire_val {
        Value::String(s) => {
            if s.len() > MAX_CODEX_TURN_METADATA_BYTES {
                return Err(CodexMetadataError::MetadataTooLarge);
            }
            serde_json::from_str(s).map_err(|e| CodexMetadataError::MalformedTurnMetadata {
                source_name: MetadataSourceName::CanonicalBody,
                reason: bounded_error_reason(&e.to_string()),
            })?
        }
        Value::Object(_) => {
            let serialized = serde_json::to_string(wire_val).map_err(|e| {
                CodexMetadataError::MalformedTurnMetadata {
                    source_name: MetadataSourceName::CanonicalBody,
                    reason: bounded_error_reason(&e.to_string()),
                }
            })?;
            if serialized.len() > MAX_CODEX_TURN_METADATA_BYTES {
                return Err(CodexMetadataError::MetadataTooLarge);
            }
            serde_json::from_str(&serialized).map_err(|e| {
                CodexMetadataError::MalformedTurnMetadata {
                    source_name: MetadataSourceName::CanonicalBody,
                    reason: bounded_error_reason(&e.to_string()),
                }
            })?
        }
        _ => {
            return Err(CodexMetadataError::MalformedTurnMetadata {
                source_name: MetadataSourceName::CanonicalBody,
                reason: "x-codex-turn-metadata in client_metadata must be a JSON string or object"
                    .into(),
            });
        }
    };

    let normalized = normalize_wire_metadata(wire, CodexIdentitySource::CanonicalClientMetadata)?;
    Ok(Some(normalized))
}

/// Parse the `x-codex-turn-metadata` header if present.
pub fn parse_turn_metadata_header(
    headers: &HeaderMap,
) -> Result<Option<CodexTurnMetadataWire>, CodexMetadataError> {
    let Some(header_value) = headers.get(X_CODEX_TURN_METADATA) else {
        return Ok(None);
    };

    let raw_bytes = header_value.as_bytes();
    if raw_bytes.len() > MAX_CODEX_TURN_METADATA_BYTES {
        return Err(CodexMetadataError::MetadataTooLarge);
    }

    let header_str =
        header_value
            .to_str()
            .map_err(|_| CodexMetadataError::MalformedTurnMetadata {
                source_name: MetadataSourceName::TurnMetadataHeader,
                reason: "Header contains non-ASCII characters".into(),
            })?;

    let wire: CodexTurnMetadataWire = serde_json::from_str(header_str).map_err(|e| {
        CodexMetadataError::MalformedTurnMetadata {
            source_name: MetadataSourceName::TurnMetadataHeader,
            reason: bounded_error_reason(&e.to_string()),
        }
    })?;

    Ok(Some(wire))
}

/// Parse flat compatibility projection from HTTP headers only.
pub fn parse_flat_header_compatibility(
    headers: &HeaderMap,
) -> Result<CodexCompatibilityProjection, CodexMetadataError> {
    let mut projection = CodexCompatibilityProjection::default();

    if let Some(val) = headers.get(X_CODEX_PARENT_THREAD_ID) {
        if let Ok(s) = val.to_str() {
            if let Ok(id) = parse_codex_opaque_id("parent_thread_id", s) {
                projection.parent_thread_id = Some(id);
            }
        }
    }

    if let Some(val) = headers.get(X_OPENAI_SUBAGENT) {
        if let Ok(s) = val.to_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() && trimmed.len() <= MAX_CODEX_ID_BYTES {
                projection.subagent_header = Some(trimmed.to_string());
            }
        }
    }

    if let Some(val) = headers.get(X_CODEX_WINDOW_ID) {
        if let Ok(s) = val.to_str() {
            if let Ok(id) = parse_codex_opaque_id("window_id", s) {
                projection.window_id = Some(id);
            }
        }
    }

    Ok(projection)
}

/// Parse flat compatibility projection from request/frame body `client_metadata` only.
pub fn parse_flat_body_compatibility(
    body: &Value,
) -> Result<CodexCompatibilityProjection, CodexMetadataError> {
    let mut projection = CodexCompatibilityProjection::default();

    if let Some(client_metadata) = body.get("client_metadata").and_then(Value::as_object) {
        if let Some(s) = client_metadata.get("session_id").and_then(Value::as_str) {
            if let Ok(id) = parse_codex_opaque_id("session_id", s) {
                projection.session_id = Some(id);
            }
        }
        if let Some(s) = client_metadata.get("thread_id").and_then(Value::as_str) {
            if let Ok(id) = parse_codex_opaque_id("thread_id", s) {
                projection.thread_id = Some(id);
            }
        }
        if let Some(s) = client_metadata.get("turn_id").and_then(Value::as_str) {
            if let Ok(id) = parse_codex_opaque_id("turn_id", s) {
                projection.turn_id = Some(id);
            }
        }
        let parent_str = client_metadata
            .get("parent_thread_id")
            .or_else(|| client_metadata.get(X_CODEX_PARENT_THREAD_ID))
            .and_then(Value::as_str);
        if let Some(s) = parent_str {
            if let Ok(id) = parse_codex_opaque_id("parent_thread_id", s) {
                projection.parent_thread_id = Some(id);
            }
        }
        let parent_turn_str = client_metadata
            .get("parent_turn_id")
            .and_then(Value::as_str);
        if let Some(s) = parent_turn_str {
            if let Ok(id) = parse_codex_opaque_id("parent_turn_id", s) {
                projection.parent_turn_id = Some(id);
            }
        }
        let root_turn_str = client_metadata.get("root_turn_id").and_then(Value::as_str);
        if let Some(s) = root_turn_str {
            if let Ok(id) = parse_codex_opaque_id("root_turn_id", s) {
                projection.root_turn_id = Some(id);
            }
        }
        let subagent_str = client_metadata
            .get("subagent")
            .or_else(|| client_metadata.get(X_OPENAI_SUBAGENT))
            .and_then(Value::as_str);
        if let Some(s) = subagent_str {
            let trimmed = s.trim();
            if !trimmed.is_empty() && trimmed.len() <= MAX_CODEX_ID_BYTES {
                projection.subagent_header = Some(trimmed.to_string());
            }
        }
        let window_str = client_metadata
            .get("window_id")
            .or_else(|| client_metadata.get(X_CODEX_WINDOW_ID))
            .and_then(Value::as_str);
        if let Some(s) = window_str {
            if let Ok(id) = parse_codex_opaque_id("window_id", s) {
                projection.window_id = Some(id);
            }
        }
    }

    Ok(projection)
}

/// Parse flat compatibility projection from headers and `client_metadata` flat fields.
pub fn parse_compatibility_projection(
    headers: &HeaderMap,
    body: &Value,
) -> Result<CodexCompatibilityProjection, CodexMetadataError> {
    let header_proj = parse_flat_header_compatibility(headers)?;
    let body_proj = parse_flat_body_compatibility(body)?;
    let mut merged = body_proj;
    if merged.parent_thread_id.is_none() {
        merged.parent_thread_id = header_proj.parent_thread_id;
    }
    if merged.parent_turn_id.is_none() {
        merged.parent_turn_id = header_proj.parent_turn_id;
    }
    if merged.root_turn_id.is_none() {
        merged.root_turn_id = header_proj.root_turn_id;
    }
    if merged.subagent_header.is_none() {
        merged.subagent_header = header_proj.subagent_header;
    }
    if merged.window_id.is_none() {
        merged.window_id = header_proj.window_id;
    }
    Ok(merged)
}

fn normalize_wire_metadata(
    wire: CodexTurnMetadataWire,
    source: CodexIdentitySource,
) -> Result<CodexTurnIdentity, CodexMetadataError> {
    let installation_id = wire
        .installation_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("installation_id", v))
        .transpose()?;
    let session_id = wire
        .session_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("session_id", v))
        .transpose()?;
    let thread_id = wire
        .thread_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("thread_id", v))
        .transpose()?;
    let turn_id = wire
        .turn_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("turn_id", v))
        .transpose()?;
    let root_turn_id = wire
        .root_turn_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("root_turn_id", v))
        .transpose()?;
    let parent_thread_id = wire
        .parent_thread_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("parent_thread_id", v))
        .transpose()?;
    let parent_turn_id = wire
        .parent_turn_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("parent_turn_id", v))
        .transpose()?;
    let forked_from_thread_id = wire
        .forked_from_thread_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("forked_from_thread_id", v))
        .transpose()?;
    let window_id = wire
        .window_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("window_id", v))
        .transpose()?;
    let context_window_id = wire
        .context_window_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_codex_opaque_id("context_window_id", v))
        .transpose()?;

    let mut unknown_fields: Vec<String> = wire
        .extra
        .keys()
        .take(MAX_UNKNOWN_METADATA_KEYS)
        .cloned()
        .collect();
    unknown_fields.sort();

    let mut known_fields = Vec::new();
    if installation_id.is_some() {
        known_fields.push("installation_id");
    }
    if session_id.is_some() {
        known_fields.push("session_id");
    }
    if thread_id.is_some() {
        known_fields.push("thread_id");
    }
    if wire.agent_name.is_some() {
        known_fields.push("agent_name");
    }
    if turn_id.is_some() {
        known_fields.push("turn_id");
    }
    if root_turn_id.is_some() {
        known_fields.push("root_turn_id");
    }
    if parent_thread_id.is_some() {
        known_fields.push("parent_thread_id");
    }
    if parent_turn_id.is_some() {
        known_fields.push("parent_turn_id");
    }
    if forked_from_thread_id.is_some() {
        known_fields.push("forked_from_thread_id");
    }
    if window_id.is_some() {
        known_fields.push("window_id");
    }
    if context_window_id.is_some() {
        known_fields.push("context_window_id");
    }
    if wire.request_kind.is_some() {
        known_fields.push("request_kind");
    }
    if wire.subagent_kind.is_some() {
        known_fields.push("subagent_kind");
    }
    if wire.thread_source.is_some() {
        known_fields.push("thread_source");
    }

    let fingerprint = schema_fingerprint(&known_fields, &unknown_fields);

    // Trust assessment:
    // If session_id and thread_id are present -> Exact or Structured
    let trust = if session_id.is_some() && thread_id.is_some() {
        if parent_thread_id.is_some() {
            CodexIdentityTrust::Exact
        } else {
            CodexIdentityTrust::Structured
        }
    } else if thread_id.is_some() || parent_thread_id.is_some() {
        CodexIdentityTrust::Partial
    } else {
        CodexIdentityTrust::None
    };

    Ok(CodexTurnIdentity {
        installation_id,
        session_id,
        thread_id,
        agent_name: wire.agent_name.map(|s| s.trim().to_string()),
        turn_id,
        root_turn_id,
        parent_thread_id,
        parent_turn_id,
        forked_from_thread_id,
        window_id,
        context_window_id,
        request_kind: wire.request_kind.map(|s| s.trim().to_string()),
        subagent_kind: wire.subagent_kind.map(|s| s.trim().to_string()),
        subagent_header: None,
        thread_source: wire.thread_source,
        source,
        trust,
        conflict_fields: Vec::new(),
        unknown_fields,
        schema_fingerprint: Some(fingerprint),
    })
}

/// Merge canonical, structured header, and flat compatibility sources with conflict detection.
pub fn merge_identity_sources(
    canonical: Option<CodexTurnIdentity>,
    structured_header: Option<CodexTurnIdentity>,
    flat_body: CodexCompatibilityProjection,
    flat_headers: CodexCompatibilityProjection,
    header_scope: CodexMetadataHeaderScope,
) -> CodexTurnIdentity {
    let mut conflicts = Vec::new();

    // ------------------------------------------------------------------------
    // Layer 1: Frame Authority Resolution & Same-Frame Conflict Check
    // ------------------------------------------------------------------------
    if let Some(can) = &canonical {
        check_field_conflict(
            "session_id",
            &can.session_id,
            &flat_body.session_id,
            &mut conflicts,
        );
        check_field_conflict(
            "thread_id",
            &can.thread_id,
            &flat_body.thread_id,
            &mut conflicts,
        );
        check_field_conflict("turn_id", &can.turn_id, &flat_body.turn_id, &mut conflicts);
        check_field_conflict(
            "parent_thread_id",
            &can.parent_thread_id,
            &flat_body.parent_thread_id,
            &mut conflicts,
        );
        check_field_conflict(
            "parent_turn_id",
            &can.parent_turn_id,
            &flat_body.parent_turn_id,
            &mut conflicts,
        );
        check_field_conflict(
            "root_turn_id",
            &can.root_turn_id,
            &flat_body.root_turn_id,
            &mut conflicts,
        );
        check_field_conflict(
            "window_id",
            &can.window_id,
            &flat_body.window_id,
            &mut conflicts,
        );
    }

    if !conflicts.is_empty() {
        let mut conflict_id = canonical.or(structured_header).unwrap_or_default();
        conflict_id.trust = CodexIdentityTrust::Conflict;
        conflict_id.conflict_fields = conflicts.into_iter().map(String::from).collect();
        return conflict_id;
    }

    let frame_identity: Option<CodexTurnIdentity> = match canonical {
        Some(mut can) => {
            // Fill missing non-conflicting fields from flat_body
            if can.session_id.is_none() && flat_body.session_id.is_some() {
                can.session_id = flat_body.session_id;
                can.source = CodexIdentitySource::MergedStructuredAndCompatibility;
            }
            if can.thread_id.is_none() && flat_body.thread_id.is_some() {
                can.thread_id = flat_body.thread_id;
                can.source = CodexIdentitySource::MergedStructuredAndCompatibility;
            }
            if can.turn_id.is_none() && flat_body.turn_id.is_some() {
                can.turn_id = flat_body.turn_id;
                can.source = CodexIdentitySource::MergedStructuredAndCompatibility;
            }
            if can.parent_thread_id.is_none() && flat_body.parent_thread_id.is_some() {
                can.parent_thread_id = flat_body.parent_thread_id;
                can.source = CodexIdentitySource::MergedStructuredAndCompatibility;
            }
            if can.parent_turn_id.is_none() && flat_body.parent_turn_id.is_some() {
                can.parent_turn_id = flat_body.parent_turn_id;
            }
            if can.root_turn_id.is_none() && flat_body.root_turn_id.is_some() {
                can.root_turn_id = flat_body.root_turn_id;
            }
            if can.window_id.is_none() && flat_body.window_id.is_some() {
                can.window_id = flat_body.window_id;
            }
            if can.subagent_header.is_none() && flat_body.subagent_header.is_some() {
                can.subagent_header = flat_body.subagent_header;
            }
            Some(can)
        }
        None => {
            let has_flat = flat_body.session_id.is_some()
                || flat_body.thread_id.is_some()
                || flat_body.turn_id.is_some()
                || flat_body.parent_thread_id.is_some()
                || flat_body.parent_turn_id.is_some()
                || flat_body.root_turn_id.is_some()
                || flat_body.subagent_header.is_some()
                || flat_body.window_id.is_some();
            if has_flat {
                let mut identity = CodexTurnIdentity {
                    session_id: flat_body.session_id,
                    thread_id: flat_body.thread_id,
                    turn_id: flat_body.turn_id,
                    parent_thread_id: flat_body.parent_thread_id,
                    parent_turn_id: flat_body.parent_turn_id,
                    root_turn_id: flat_body.root_turn_id,
                    window_id: flat_body.window_id,
                    subagent_header: flat_body.subagent_header,
                    source: CodexIdentitySource::FlatCompatibilityHeaders,
                    trust: CodexIdentityTrust::Partial,
                    ..Default::default()
                };
                let mut known = Vec::new();
                if identity.session_id.is_some() {
                    known.push("session_id");
                }
                if identity.thread_id.is_some() {
                    known.push("thread_id");
                }
                if identity.turn_id.is_some() {
                    known.push("turn_id");
                }
                if identity.parent_thread_id.is_some() {
                    known.push("parent_thread_id");
                }
                if identity.parent_turn_id.is_some() {
                    known.push("parent_turn_id");
                }
                if identity.root_turn_id.is_some() {
                    known.push("root_turn_id");
                }
                if identity.window_id.is_some() {
                    known.push("window_id");
                }
                if identity.subagent_header.is_some() {
                    known.push("subagent_header");
                }
                identity.schema_fingerprint = Some(schema_fingerprint(&known, &[]));
                Some(identity)
            } else {
                None
            }
        }
    };

    // ------------------------------------------------------------------------
    // Layer 2: Scope-Dependent Header Integration
    // ------------------------------------------------------------------------
    match header_scope {
        CodexMetadataHeaderScope::SameRequest => {
            // Under SameRequest (HTTP), headers and body belong to the exact same request.
            if let Some(frame) = &frame_identity {
                if let Some(hdr) = &structured_header {
                    check_field_conflict(
                        "session_id",
                        &frame.session_id,
                        &hdr.session_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "thread_id",
                        &frame.thread_id,
                        &hdr.thread_id,
                        &mut conflicts,
                    );
                    check_field_conflict("turn_id", &frame.turn_id, &hdr.turn_id, &mut conflicts);
                    check_field_conflict(
                        "parent_thread_id",
                        &frame.parent_thread_id,
                        &hdr.parent_thread_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "parent_turn_id",
                        &frame.parent_turn_id,
                        &hdr.parent_turn_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "root_turn_id",
                        &frame.root_turn_id,
                        &hdr.root_turn_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "context_window_id",
                        &frame.context_window_id,
                        &hdr.context_window_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "window_id",
                        &frame.window_id,
                        &hdr.window_id,
                        &mut conflicts,
                    );
                }
                check_field_conflict(
                    "parent_thread_id",
                    &frame.parent_thread_id,
                    &flat_headers.parent_thread_id,
                    &mut conflicts,
                );
                check_field_conflict(
                    "window_id",
                    &frame.window_id,
                    &flat_headers.window_id,
                    &mut conflicts,
                );
            } else if let Some(hdr) = &structured_header {
                check_field_conflict(
                    "parent_thread_id",
                    &hdr.parent_thread_id,
                    &flat_headers.parent_thread_id,
                    &mut conflicts,
                );
                check_field_conflict(
                    "window_id",
                    &hdr.window_id,
                    &flat_headers.window_id,
                    &mut conflicts,
                );
            }

            if !conflicts.is_empty() {
                let mut conflict_id = frame_identity.or(structured_header).unwrap_or_default();
                conflict_id.trust = CodexIdentityTrust::Conflict;
                conflict_id.conflict_fields = conflicts.into_iter().map(String::from).collect();
                return conflict_id;
            }

            // Merge frame_identity, structured_header, and flat_headers for HTTP
            let (mut primary, has_structured) = match (frame_identity, structured_header) {
                (Some(mut frame), Some(hdr)) => {
                    if frame.installation_id.is_none() {
                        frame.installation_id = hdr.installation_id;
                    }
                    if frame.agent_name.is_none() {
                        frame.agent_name = hdr.agent_name;
                    }
                    if frame.forked_from_thread_id.is_none() {
                        frame.forked_from_thread_id = hdr.forked_from_thread_id;
                    }
                    if frame.window_id.is_none() {
                        frame.window_id = hdr.window_id;
                    }
                    if frame.context_window_id.is_none() {
                        frame.context_window_id = hdr.context_window_id;
                    }
                    if frame.request_kind.is_none() {
                        frame.request_kind = hdr.request_kind;
                    }
                    if frame.subagent_kind.is_none() {
                        frame.subagent_kind = hdr.subagent_kind;
                    }
                    if frame.thread_source.is_none() {
                        frame.thread_source = hdr.thread_source;
                    }
                    if frame.session_id.is_none() {
                        frame.session_id = hdr.session_id;
                    }
                    if frame.thread_id.is_none() {
                        frame.thread_id = hdr.thread_id;
                    }
                    if frame.turn_id.is_none() {
                        frame.turn_id = hdr.turn_id;
                    }
                    if frame.parent_thread_id.is_none() {
                        frame.parent_thread_id = hdr.parent_thread_id;
                    }
                    if frame.parent_turn_id.is_none() {
                        frame.parent_turn_id = hdr.parent_turn_id;
                    }
                    if frame.root_turn_id.is_none() {
                        frame.root_turn_id = hdr.root_turn_id;
                    }
                    frame.source = CodexIdentitySource::MergedStructuredAndCompatibility;
                    (frame, true)
                }
                (Some(frame), None) => {
                    let is_struct = frame.source != CodexIdentitySource::FlatCompatibilityHeaders;
                    (frame, is_struct)
                }
                (None, Some(hdr)) => (hdr, true),
                (None, None) => (CodexTurnIdentity::default(), false),
            };

            if has_structured {
                if primary.parent_thread_id.is_none() && flat_headers.parent_thread_id.is_some() {
                    primary.parent_thread_id = flat_headers.parent_thread_id;
                    primary.source = CodexIdentitySource::MergedStructuredAndCompatibility;
                }
                if primary.window_id.is_none() && flat_headers.window_id.is_some() {
                    primary.window_id = flat_headers.window_id;
                }
                if primary.subagent_header.is_none() && flat_headers.subagent_header.is_some() {
                    primary.subagent_header = flat_headers.subagent_header;
                }
            } else if primary.source == CodexIdentitySource::FlatCompatibilityHeaders {
                if primary.parent_thread_id.is_none() && flat_headers.parent_thread_id.is_some() {
                    primary.parent_thread_id = flat_headers.parent_thread_id;
                }
                if primary.window_id.is_none() && flat_headers.window_id.is_some() {
                    primary.window_id = flat_headers.window_id;
                }
                if primary.subagent_header.is_none() && flat_headers.subagent_header.is_some() {
                    primary.subagent_header = flat_headers.subagent_header;
                }
            } else {
                // Flat headers only
                let has_flat = flat_headers.session_id.is_some()
                    || flat_headers.thread_id.is_some()
                    || flat_headers.turn_id.is_some()
                    || flat_headers.parent_thread_id.is_some()
                    || flat_headers.parent_turn_id.is_some()
                    || flat_headers.root_turn_id.is_some()
                    || flat_headers.subagent_header.is_some()
                    || flat_headers.window_id.is_some();
                if has_flat {
                    primary = CodexTurnIdentity {
                        session_id: flat_headers.session_id,
                        thread_id: flat_headers.thread_id,
                        turn_id: flat_headers.turn_id,
                        parent_thread_id: flat_headers.parent_thread_id,
                        parent_turn_id: flat_headers.parent_turn_id,
                        root_turn_id: flat_headers.root_turn_id,
                        window_id: flat_headers.window_id,
                        subagent_header: flat_headers.subagent_header,
                        source: CodexIdentitySource::FlatCompatibilityHeaders,
                        trust: CodexIdentityTrust::Partial,
                        ..Default::default()
                    };
                }
            }

            if primary.session_id.is_some() && primary.thread_id.is_some() {
                if primary.parent_thread_id.is_some() {
                    primary.trust = CodexIdentityTrust::Exact;
                } else if primary.trust == CodexIdentityTrust::None
                    || primary.trust == CodexIdentityTrust::Partial
                {
                    primary.trust = CodexIdentityTrust::Structured;
                }
            }

            primary
        }
        CodexMetadataHeaderScope::ConnectionProjection => {
            // Under ConnectionProjection (WebSocket), if the frame has its own turn identity,
            // connection handshake headers MUST NOT mutate or inject graph/turn identity!
            if let Some(mut frame) = frame_identity {
                // Enrich only non-graph connection-level metadata from structured header if missing
                if let Some(hdr) = structured_header {
                    if frame.installation_id.is_none() {
                        frame.installation_id = hdr.installation_id;
                    }
                    if frame.agent_name.is_none() {
                        frame.agent_name = hdr.agent_name;
                    }
                    if frame.request_kind.is_none() {
                        frame.request_kind = hdr.request_kind;
                    }
                    if frame.thread_source.is_none() {
                        frame.thread_source = hdr.thread_source;
                    }
                }

                if frame.session_id.is_some() && frame.thread_id.is_some() {
                    if frame.parent_thread_id.is_some() {
                        frame.trust = CodexIdentityTrust::Exact;
                    } else if frame.trust == CodexIdentityTrust::None
                        || frame.trust == CodexIdentityTrust::Partial
                    {
                        frame.trust = CodexIdentityTrust::Structured;
                    }
                }

                frame
            } else {
                // Frame had no turn identity whatsoever: fallback to connection handshake headers
                if let Some(hdr) = &structured_header {
                    check_field_conflict(
                        "parent_thread_id",
                        &hdr.parent_thread_id,
                        &flat_headers.parent_thread_id,
                        &mut conflicts,
                    );
                    check_field_conflict(
                        "window_id",
                        &hdr.window_id,
                        &flat_headers.window_id,
                        &mut conflicts,
                    );
                }

                if !conflicts.is_empty() {
                    let mut conflict_id = structured_header.unwrap_or_default();
                    conflict_id.trust = CodexIdentityTrust::Conflict;
                    conflict_id.conflict_fields = conflicts.into_iter().map(String::from).collect();
                    return conflict_id;
                }

                if let Some(mut hdr) = structured_header {
                    if hdr.parent_thread_id.is_none() && flat_headers.parent_thread_id.is_some() {
                        hdr.parent_thread_id = flat_headers.parent_thread_id;
                        hdr.source = CodexIdentitySource::MergedStructuredAndCompatibility;
                    }
                    if hdr.window_id.is_none() && flat_headers.window_id.is_some() {
                        hdr.window_id = flat_headers.window_id;
                    }
                    if hdr.subagent_header.is_none() && flat_headers.subagent_header.is_some() {
                        hdr.subagent_header = flat_headers.subagent_header;
                    }
                    if hdr.session_id.is_some() && hdr.thread_id.is_some() {
                        if hdr.parent_thread_id.is_some() {
                            hdr.trust = CodexIdentityTrust::Exact;
                        } else if hdr.trust == CodexIdentityTrust::None
                            || hdr.trust == CodexIdentityTrust::Partial
                        {
                            hdr.trust = CodexIdentityTrust::Structured;
                        }
                    }
                    hdr
                } else {
                    let has_flat = flat_headers.session_id.is_some()
                        || flat_headers.thread_id.is_some()
                        || flat_headers.turn_id.is_some()
                        || flat_headers.parent_thread_id.is_some()
                        || flat_headers.parent_turn_id.is_some()
                        || flat_headers.root_turn_id.is_some()
                        || flat_headers.subagent_header.is_some()
                        || flat_headers.window_id.is_some();
                    if has_flat {
                        let mut identity = CodexTurnIdentity {
                            session_id: flat_headers.session_id,
                            thread_id: flat_headers.thread_id,
                            turn_id: flat_headers.turn_id,
                            parent_thread_id: flat_headers.parent_thread_id,
                            parent_turn_id: flat_headers.parent_turn_id,
                            root_turn_id: flat_headers.root_turn_id,
                            window_id: flat_headers.window_id,
                            subagent_header: flat_headers.subagent_header,
                            source: CodexIdentitySource::FlatCompatibilityHeaders,
                            trust: CodexIdentityTrust::Partial,
                            ..Default::default()
                        };
                        let mut known = Vec::new();
                        if identity.session_id.is_some() {
                            known.push("session_id");
                        }
                        if identity.thread_id.is_some() {
                            known.push("thread_id");
                        }
                        if identity.turn_id.is_some() {
                            known.push("turn_id");
                        }
                        if identity.parent_thread_id.is_some() {
                            known.push("parent_thread_id");
                        }
                        if identity.parent_turn_id.is_some() {
                            known.push("parent_turn_id");
                        }
                        if identity.root_turn_id.is_some() {
                            known.push("root_turn_id");
                        }
                        if identity.window_id.is_some() {
                            known.push("window_id");
                        }
                        if identity.subagent_header.is_some() {
                            known.push("subagent_header");
                        }
                        identity.schema_fingerprint = Some(schema_fingerprint(&known, &[]));
                        identity
                    } else {
                        CodexTurnIdentity::default()
                    }
                }
            }
        }
    }
}

fn check_field_conflict(
    field_name: &'static str,
    a: &Option<CodexOpaqueId>,
    b: &Option<CodexOpaqueId>,
    conflicts: &mut Vec<&'static str>,
) {
    if let (Some(val_a), Some(val_b)) = (a, b) {
        if val_a != val_b {
            conflicts.push(field_name);
        }
    }
}

/// Compute a schema fingerprint from present known and unknown field names.
pub fn schema_fingerprint(
    known_present_fields: &[&'static str],
    unknown_fields: &[String],
) -> String {
    use sha2::{Digest, Sha256};
    let mut all_fields = Vec::new();
    for f in known_present_fields {
        all_fields.push(*f);
    }
    for u in unknown_fields {
        all_fields.push(u.as_str());
    }
    all_fields.sort_unstable();

    let combined = all_fields.join(",");
    let digest = Sha256::digest(combined.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn bounded_error_reason(msg: &str) -> String {
    let max_len = 128;
    if msg.len() <= max_len {
        msg.to_string()
    } else {
        format!("{}...", &msg[..max_len])
    }
}

// ---------------------------------------------------------------------------
// Capability Model (§6)
// ---------------------------------------------------------------------------

/// Codex release channel classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexReleaseChannel {
    Stable,
    Prerelease,
    Head,
    #[default]
    Unknown,
}

/// Overall support level for a Codex installation / protocol observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexSupportLevel {
    HeadObserved,
    StableQualified,
    PrereleaseQualified,
    BackwardCompatible,
    LegacyFallback,
    #[default]
    Unsupported,
}

/// Resolved capability profile for Codex multi-agent and turn identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCapabilityProfile {
    pub observed_version: Option<String>,
    pub channel: CodexReleaseChannel,

    pub multi_agent_v2: bool,
    pub native_subagent_defaults: bool,

    pub canonical_turn_metadata: bool,
    pub thread_identity: bool,
    pub parent_thread_identity: bool,
    pub parent_turn_identity: bool,
    pub root_turn_identity: bool,
    pub context_window_identity: bool,

    pub subagent_activity_identity: bool,
    pub agent_status_tracking: bool,

    pub fork_none: bool,
    pub fork_full_history: bool,
    pub fork_last_n_turns: bool,

    pub support_level: CodexSupportLevel,
}

/// Resolve Codex capabilities from installed version string, observed data-plane metadata,
/// and optional app-server activity signals.
pub fn resolve_codex_capabilities(
    installed_version: Option<&str>,
    observed_identity: Option<&CodexTurnIdentity>,
    _observed_activity: Option<&crate::subagent_graph::CodexSubagentActivity>,
) -> CodexCapabilityProfile {
    let mut profile = CodexCapabilityProfile {
        observed_version: installed_version.map(|v| v.trim().to_string()),
        ..Default::default()
    };

    // Determine channel & version capabilities from version string if available
    let parsed_semver = installed_version.and_then(parse_version_triplet);

    if let Some((major, minor, _patch, prerelease)) = parsed_semver {
        profile.channel = if prerelease.is_some() {
            if minor >= 150 || major > 0 {
                CodexReleaseChannel::Head
            } else {
                CodexReleaseChannel::Prerelease
            }
        } else {
            CodexReleaseChannel::Stable
        };

        // MultiAgentV2 is supported from 0.147.0 onwards
        let is_v2_eligible = (major == 0 && minor >= 147) || major > 0;
        if is_v2_eligible {
            profile.multi_agent_v2 = true;
            profile.native_subagent_defaults = true;
            profile.fork_none = true;
            profile.fork_full_history = true;
            profile.fork_last_n_turns = true;
            profile.agent_status_tracking = minor >= 148 || major > 0;
        }
    } else if let Some(version_str) = installed_version {
        if version_str.contains("head")
            || version_str.contains("alpha")
            || version_str.contains("beta")
        {
            profile.channel = CodexReleaseChannel::Head;
        } else {
            profile.channel = CodexReleaseChannel::Unknown;
        }
    }

    // Inspect observed data-plane identity
    if let Some(identity) = observed_identity {
        if identity.source == CodexIdentitySource::CanonicalClientMetadata {
            profile.canonical_turn_metadata = true;
        }
        if identity.session_id.is_some() && identity.thread_id.is_some() {
            profile.thread_identity = true;
        }
        if identity.parent_thread_id.is_some() {
            profile.parent_thread_identity = true;
        }
        if identity.parent_turn_id.is_some() {
            profile.parent_turn_identity = true;
        }
        if identity.root_turn_id.is_some() {
            profile.root_turn_identity = true;
        }
        if identity.context_window_id.is_some() {
            profile.context_window_identity = true;
        }
        if identity.subagent_kind.is_some() || identity.subagent_header.is_some() {
            profile.multi_agent_v2 = true;
        }
    }

    // Assign overall support level
    profile.support_level = if profile.context_window_identity
        || profile.channel == CodexReleaseChannel::Head
    {
        CodexSupportLevel::HeadObserved
    } else if profile.thread_identity && profile.channel == CodexReleaseChannel::Stable {
        CodexSupportLevel::StableQualified
    } else if profile.channel == CodexReleaseChannel::Prerelease {
        CodexSupportLevel::PrereleaseQualified
    } else if profile.multi_agent_v2 || profile.thread_identity || profile.parent_thread_identity {
        CodexSupportLevel::BackwardCompatible
    } else if installed_version.is_some() || observed_identity.is_some() {
        CodexSupportLevel::LegacyFallback
    } else {
        CodexSupportLevel::Unsupported
    };

    profile
}

fn parse_version_triplet(s: &str) -> Option<(u64, u64, u64, Option<&str>)> {
    let clean = s
        .trim()
        .trim_start_matches('v')
        .trim_start_matches("codex-cli ");
    let (ver_part, prerelease) = if let Some(idx) = clean.find('-') {
        (&clean[..idx], Some(&clean[idx + 1..]))
    } else {
        (clean, None)
    };

    let parts: Vec<&str> = ver_part.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = parts[1].parse::<u64>().ok()?;
    let patch = parts[2].parse::<u64>().ok()?;
    Some((major, minor, patch, prerelease))
}

// ---------------------------------------------------------------------------
// Unit Tests (§22.1 & §22.2)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn parses_latest_structured_identity_with_context_window_id() {
        let raw_json = json!({
            "session_id": "S_01",
            "thread_id": "T_CHILD",
            "turn_id": "turn_02",
            "parent_thread_id": "T_PARENT",
            "parent_turn_id": "turn_01",
            "root_turn_id": "turn_root",
            "context_window_id": "cw_99",
            "subagent_kind": "thread_spawn",
            "future_field": "ignored_value"
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );

        let body = json!({ "model": "vlm-codex", "input": [] });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).expect("extraction succeeds");

        assert_eq!(identity.session_id.as_deref(), Some("S_01"));
        assert_eq!(identity.thread_id.as_deref(), Some("T_CHILD"));
        assert_eq!(identity.parent_thread_id.as_deref(), Some("T_PARENT"));
        assert_eq!(identity.turn_id.as_deref(), Some("turn_02"));
        assert_eq!(identity.parent_turn_id.as_deref(), Some("turn_01"));
        assert_eq!(identity.root_turn_id.as_deref(), Some("turn_root"));
        assert_eq!(identity.context_window_id.as_deref(), Some("cw_99"));
        assert_eq!(identity.subagent_kind.as_deref(), Some("thread_spawn"));
        assert_eq!(identity.unknown_fields, vec!["future_field".to_string()]);
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
        assert_eq!(identity.source, CodexIdentitySource::TurnMetadataHeader);
        assert!(identity.schema_fingerprint.is_some());
    }

    #[test]
    fn parses_stable_identity_without_context_window_id() {
        let raw_json = json!({
            "session_id": "S_STABLE",
            "thread_id": "T_STABLE_CHILD",
            "parent_thread_id": "T_STABLE_PARENT",
            "turn_id": "turn_10"
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.context_window_id, None);
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
    }

    #[test]
    fn parses_older_identity_without_root_or_context_window() {
        let raw_json = json!({
            "session_id": "S_OLD",
            "thread_id": "T_OLD",
            "parent_thread_id": "T_OLD_P"
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.root_turn_id, None);
        assert_eq!(identity.context_window_id, None);
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
    }

    #[test]
    fn missing_codex_metadata_returns_default_none_identity() {
        let headers = HeaderMap::new();
        let body = json!({ "model": "vlm-chat" });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.trust, CodexIdentityTrust::None);
        assert_eq!(identity.source, CodexIdentitySource::None);
        assert!(identity.thread_id.is_none());
    }

    #[test]
    fn future_fields_are_tolerated_and_bounded() {
        let mut map = serde_json::Map::new();
        map.insert("session_id".into(), json!("S_FUTURE"));
        map.insert("thread_id".into(), json!("T_FUTURE"));
        for i in 0..50 {
            map.insert(format!("extra_key_{i:02}"), json!("some_val"));
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&Value::Object(map).to_string()).unwrap(),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.unknown_fields.len(), MAX_UNKNOWN_METADATA_KEYS);
    }

    #[test]
    fn oversized_turn_metadata_is_rejected_without_panic() {
        let large_string = "a".repeat(MAX_CODEX_TURN_METADATA_BYTES + 100);
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&format!("{{\"session_id\":\"{large_string}\"}}")).unwrap(),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let err = extract_codex_turn_identity(sources).unwrap_err();
        assert_eq!(err, CodexMetadataError::MetadataTooLarge);
    }

    #[test]
    fn malformed_turn_metadata_returns_bounded_error() {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_static("not_valid_json"),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let err = extract_codex_turn_identity(sources).unwrap_err();
        match err {
            CodexMetadataError::MalformedTurnMetadata {
                source_name,
                reason,
            } => {
                assert_eq!(source_name, MetadataSourceName::TurnMetadataHeader);
                assert!(!reason.contains("not_valid_json"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn opaque_id_rejects_empty_control_and_oversized_values() {
        assert!(parse_codex_opaque_id("test", "   ").is_err());
        assert!(parse_codex_opaque_id("test", "val\nwith_ctrl").is_err());
        let too_long = "x".repeat(MAX_CODEX_ID_BYTES + 1);
        assert!(parse_codex_opaque_id("test", &too_long).is_err());

        let valid = parse_codex_opaque_id("test", " valid_id_123 ").unwrap();
        assert_eq!(valid.as_str(), "valid_id_123");
    }

    #[test]
    fn compatible_structured_and_flat_sources_merge_cleanly() {
        let raw_json = json!({
            "session_id": "S_MERGE",
            "thread_id": "T_CHILD_1",
            "parent_thread_id": "T_PARENT_1"
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );
        headers.insert(
            X_CODEX_PARENT_THREAD_ID,
            HeaderValue::from_static("T_PARENT_1"),
        );
        headers.insert(X_OPENAI_SUBAGENT, HeaderValue::from_static("1"));

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
        assert_eq!(identity.parent_thread_id.as_deref(), Some("T_PARENT_1"));
        assert_eq!(identity.subagent_header.as_deref(), Some("1"));
    }

    #[test]
    fn conflicting_parent_thread_id_marks_identity_conflict() {
        let raw_json = json!({
            "session_id": "S_CONF",
            "thread_id": "T_CHILD_C",
            "parent_thread_id": "T_PARENT_A"
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );
        headers.insert(
            X_CODEX_PARENT_THREAD_ID,
            HeaderValue::from_static("T_PARENT_B"), // Conflict!
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity
            .conflict_fields
            .contains(&"parent_thread_id".to_string()));
    }

    #[test]
    fn flat_projection_only_fills_missing_non_conflicting_values() {
        let raw_json = json!({
            "session_id": "S_NOPARENT",
            "thread_id": "T_NOPARENT"
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            X_CODEX_TURN_METADATA,
            HeaderValue::from_str(&raw_json.to_string()).unwrap(),
        );
        headers.insert(
            X_CODEX_PARENT_THREAD_ID,
            HeaderValue::from_static("T_FILLED_PARENT"),
        );

        let body = json!({});
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(
            identity.parent_thread_id.as_deref(),
            Some("T_FILLED_PARENT")
        );
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
    }

    #[test]
    fn real_codex_0_149_client_metadata_json_string_fixture() {
        let body = json!({
            "client_metadata": {
                "session_id": "0195328e-8765-7123-9876-0123456789ab",
                "thread_id": "th_child_123",
                "turn_id": "turn_002",
                "x-codex-parent-thread-id": "th_parent_001",
                "x-openai-subagent": "collab_spawn",
                "x-codex-window-id": "win_01",
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child_123\",\"turn_id\":\"turn_002\",\"parent_thread_id\":\"th_parent_001\",\"parent_turn_id\":\"turn_001\",\"root_turn_id\":\"turn_000\",\"subagent_kind\":\"collab_spawn\"}"
            }
        });
        let headers = HeaderMap::new();
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(
            identity.session_id.as_deref(),
            Some("0195328e-8765-7123-9876-0123456789ab")
        );
        assert_eq!(identity.thread_id.as_deref(), Some("th_child_123"));
        assert_eq!(identity.parent_thread_id.as_deref(), Some("th_parent_001"));
        assert_eq!(identity.parent_turn_id.as_deref(), Some("turn_001"));
        assert_eq!(identity.root_turn_id.as_deref(), Some("turn_000"));
        assert_eq!(identity.subagent_kind.as_deref(), Some("collab_spawn"));
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
        assert_eq!(
            identity.source,
            CodexIdentitySource::CanonicalClientMetadata
        );
    }

    #[test]
    fn real_codex_0_150_context_window_id_fixture() {
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"S_150\",\"thread_id\":\"T_150\",\"parent_thread_id\":\"T_P150\",\"parent_turn_id\":\"turn_p\",\"context_window_id\":\"cw_alpha_8\"}"
            }
        });
        let headers = HeaderMap::new();
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.context_window_id.as_deref(), Some("cw_alpha_8"));
        assert_eq!(identity.parent_turn_id.as_deref(), Some("turn_p"));
        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
    }

    #[test]
    fn arbitrary_body_metadata_is_not_treated_as_official_authority() {
        let body = json!({
            "metadata": {
                "session_id": "S_FAKE",
                "thread_id": "T_FAKE",
                "parent_thread_id": "T_FAKE_P"
            },
            "turn_metadata": {
                "session_id": "S_FAKE_2",
                "thread_id": "T_FAKE_2"
            }
        });
        let headers = HeaderMap::new();
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses);

        let identity = extract_codex_turn_identity(sources).unwrap();
        assert_eq!(identity.trust, CodexIdentityTrust::None);
        assert_eq!(identity.source, CodexIdentitySource::None);
    }

    #[test]
    fn future_unknown_version_with_observed_metadata_is_not_rejected() {
        let raw_json = json!({
            "session_id": "S_999",
            "thread_id": "T_999",
            "context_window_id": "cw_999"
        });
        let id = normalize_wire_metadata(
            serde_json::from_value(raw_json).unwrap(),
            CodexIdentitySource::TurnMetadataHeader,
        )
        .unwrap();

        let caps = resolve_codex_capabilities(Some("codex-cli 0.999.0"), Some(&id), None);
        assert!(caps.thread_identity);
        assert!(caps.context_window_identity);
        assert_eq!(caps.support_level, CodexSupportLevel::HeadObserved);
    }

    #[test]
    fn context_window_capability_is_inferred_from_observed_field() {
        let raw_json = json!({
            "session_id": "S",
            "thread_id": "T",
            "context_window_id": "CW"
        });
        let id = normalize_wire_metadata(
            serde_json::from_value(raw_json).unwrap(),
            CodexIdentitySource::TurnMetadataHeader,
        )
        .unwrap();

        let caps = resolve_codex_capabilities(Some("0.149.0"), Some(&id), None);
        assert!(caps.context_window_identity);
    }

    #[test]
    fn native_config_capability_can_still_use_centralized_version_mapping() {
        let caps = resolve_codex_capabilities(Some("0.149.0"), None, None);
        assert!(caps.multi_agent_v2);
        assert!(caps.native_subagent_defaults);
        assert_eq!(caps.channel, CodexReleaseChannel::Stable);
        assert_eq!(caps.support_level, CodexSupportLevel::BackwardCompatible);
    }

    #[test]
    fn malformed_version_does_not_disable_observed_protocol_features() {
        let raw_json = json!({
            "session_id": "S",
            "thread_id": "T",
            "parent_thread_id": "P"
        });
        let id = normalize_wire_metadata(
            serde_json::from_value(raw_json).unwrap(),
            CodexIdentitySource::TurnMetadataHeader,
        )
        .unwrap();

        let caps = resolve_codex_capabilities(Some("invalid-ver-format"), Some(&id), None);
        assert!(caps.thread_identity);
        assert!(caps.parent_thread_identity);
        assert_eq!(caps.support_level, CodexSupportLevel::BackwardCompatible);
    }

    #[test]
    fn http_canonical_and_structured_header_parent_mismatch_is_conflict() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"parent_thread_id\":\"th_parent_2\"}".parse().unwrap(),
        );
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"parent_thread_id\":\"th_parent_1\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::SameRequest);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity
            .conflict_fields
            .contains(&"parent_thread_id".to_string()));
    }

    #[test]
    fn websocket_per_turn_canonical_outranks_stale_connection_projection() {
        let mut headers = HeaderMap::new();
        // Handshake header on the connection had root thread and empty turn_id from prewarm
        headers.insert(
            "x-codex-turn-metadata",
            "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_root\",\"turn_id\":\"\",\"request_kind\":\"prewarm\"}".parse().unwrap(),
        );
        // Per-turn frame body has canonical subagent child metadata
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"turn_id\":\"turn_child_1\",\"parent_thread_id\":\"th_root\",\"request_kind\":\"turn\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Exact);
        assert_eq!(identity.thread_id.as_deref(), Some("th_child"));
        assert_eq!(identity.parent_thread_id.as_deref(), Some("th_root"));
        assert_eq!(identity.turn_id.as_deref(), Some("turn_child_1"));
        assert!(identity.conflict_fields.is_empty());
    }

    #[test]
    fn websocket_stale_window_header_does_not_conflict_with_frame_canonical() {
        let mut headers = HeaderMap::new();
        // Handshake header has stale window id W1
        headers.insert(X_CODEX_WINDOW_ID, "win_01_handshake".parse().unwrap());
        // Frame body has canonical window id W2
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child_1\",\"turn_id\":\"turn_1\",\"window_id\":\"win_02_frame\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_ne!(identity.trust, CodexIdentityTrust::Conflict);
        assert_eq!(identity.window_id.as_deref(), Some("win_02_frame"));
        assert!(identity.conflict_fields.is_empty());
    }

    #[test]
    fn websocket_stale_parent_header_does_not_conflict_with_frame_canonical() {
        let mut headers = HeaderMap::new();
        // Handshake header has stale parent thread id P1
        headers.insert(X_CODEX_PARENT_THREAD_ID, "th_old_parent".parse().unwrap());
        // Frame body has canonical parent thread id P2
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child_2\",\"turn_id\":\"turn_2\",\"parent_thread_id\":\"th_new_parent\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_ne!(identity.trust, CodexIdentityTrust::Conflict);
        assert_eq!(identity.parent_thread_id.as_deref(), Some("th_new_parent"));
        assert!(identity.conflict_fields.is_empty());
    }

    #[test]
    fn websocket_same_frame_flat_metadata_conflict_still_fails_closed() {
        let headers = HeaderMap::new();
        // Frame body has contradictory canonical parent (P1) and flat client_metadata parent (P2)
        let body = json!({
            "client_metadata": {
                "parent_thread_id": "th_flat_parent_2",
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child_3\",\"turn_id\":\"turn_3\",\"parent_thread_id\":\"th_canonical_parent_1\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity
            .conflict_fields
            .contains(&"parent_thread_id".to_string()));
    }

    #[test]
    fn websocket_same_frame_parent_turn_conflict_fails_closed() {
        let headers = HeaderMap::new();
        let body = json!({
            "client_metadata": {
                "parent_turn_id": "TURN_B",
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"parent_thread_id\":\"th_parent\",\"parent_turn_id\":\"TURN_A\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity
            .conflict_fields
            .contains(&"parent_turn_id".to_string()));
    }

    #[test]
    fn http_same_request_parent_turn_conflict_fails_closed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"parent_thread_id\":\"th_parent\",\"parent_turn_id\":\"TURN_HDR\"}".parse().unwrap(),
        );
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child\",\"parent_thread_id\":\"th_parent\",\"parent_turn_id\":\"TURN_BODY\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::SameRequest);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity
            .conflict_fields
            .contains(&"parent_turn_id".to_string()));
    }

    #[test]
    fn websocket_root_frame_does_not_inherit_stale_handshake_parent() {
        let mut headers = HeaderMap::new();
        // Handshake header has stale child parent thread ID
        headers.insert(
            X_CODEX_PARENT_THREAD_ID,
            "th_old_child_parent".parse().unwrap(),
        );
        // Frame canonical is a root turn with no parent
        let body = json!({
            "client_metadata": {
                "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_root_new\",\"turn_id\":\"turn_new\"}"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::ConnectionProjection);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.parent_thread_id, None);
        assert_eq!(identity.trust, CodexIdentityTrust::Structured);
        assert!(identity.conflict_fields.is_empty());
    }

    #[test]
    fn http_structured_header_and_flat_body_conflict_fails_closed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            "{\"session_id\":\"S_HDR\",\"thread_id\":\"T_HDR\"}"
                .parse()
                .unwrap(),
        );
        let body = json!({
            "client_metadata": {
                "session_id": "S_BODY",
                "thread_id": "T_BODY"
            }
        });
        let sources = CodexMetadataSources::new(&headers, &body, RuntimeEndpoint::Responses)
            .with_scope(CodexMetadataHeaderScope::SameRequest);
        let identity = extract_codex_turn_identity(sources).unwrap();

        assert_eq!(identity.trust, CodexIdentityTrust::Conflict);
        assert!(identity.conflict_fields.contains(&"session_id".to_string()));
    }
}
