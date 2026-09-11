//! Shared HTTP request boundary (plan M2): request body decoding, size
//! limits, and the error envelope Desktop's proxy already produces for
//! boundary failures. Moved verbatim out of `src-tauri/src/proxy.rs` so
//! Desktop and the headless daemon decode requests with the exact same code
//! instead of two implementations that merely look alike.
//!
//! This is boundary-layer only: it has no opinion on what happens to a
//! successfully-decoded request afterward. `forward_response()` and its
//! adapter/continuation/compaction pipeline stay in `src-tauri` untouched —
//! that's M3+.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::RuntimeError;

pub const MAX_REQUEST_BODY_BYTES: usize = 300 * 1024 * 1024;

#[derive(Debug)]
pub enum RequestBodyError {
    WireTooLarge,
    DecodedTooLarge,
    /// Any `Content-Encoding` other than identity. Never decompressed.
    UnsupportedEncoding(String),
    Invalid(String),
}

impl std::fmt::Display for RequestBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WireTooLarge => write!(
                formatter,
                "request body exceeds the Vellum wire limit of 300 MiB"
            ),
            Self::DecodedTooLarge => write!(
                formatter,
                "decoded request body exceeds the Vellum limit of 300 MiB"
            ),
            Self::UnsupportedEncoding(encoding) => write!(
                formatter,
                "Unsupported request Content-Encoding '{encoding}'; Vellum accepts only identity"
            ),
            Self::Invalid(cause) => formatter.write_str(cause),
        }
    }
}

pub fn error_envelope(
    status: u16,
    provider: &str,
    model: &str,
    endpoint: &str,
    cause: &str,
) -> Value {
    json!({
        "error": {
            "message": format!(
                "Vellum local proxy failed. Provider: {provider}; model: {model}; endpoint: {endpoint}; upstream_status: HTTP {status}; cause: {cause}"
            ),
            "type": "vellum_proxy_error",
            "code": status
        }
    })
}

pub fn request_body_error_response(endpoint: &str, error: RequestBodyError) -> Response {
    if let RequestBodyError::UnsupportedEncoding(encoding) = &error {
        return RuntimeError::UnsupportedMedia(format!(
            "Unsupported request Content-Encoding '{encoding}'; Vellum accepts only identity"
        ))
        .into_error_response();
    }
    let too_large = matches!(
        error,
        RequestBodyError::WireTooLarge | RequestBodyError::DecodedTooLarge
    );
    let status = if too_large {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(error_envelope(
            status.as_u16(),
            "Vellum local proxy",
            "unknown",
            endpoint,
            &error.to_string(),
        )),
    )
        .into_response()
}

pub fn parse_json_request(headers: &HeaderMap, body: &[u8]) -> Result<Value, RequestBodyError> {
    let decoded = decode_request_body(headers, body)?;
    serde_json::from_slice::<Value>(&decoded).map_err(|error| {
        RequestBodyError::Invalid({
            if decoded.is_empty() {
                "Vellum received an empty HTTP request body after the Codex WebSocket fallback"
                    .to_string()
            } else {
                format!("Invalid JSON request body: {error}")
            }
        })
    })
}

pub fn decode_request_body(headers: &HeaderMap, body: &[u8]) -> Result<Vec<u8>, RequestBodyError> {
    decode_request_body_with_limit(headers, body, MAX_REQUEST_BODY_BYTES)
}

pub fn decode_request_body_with_limit(
    headers: &HeaderMap,
    body: &[u8],
    max_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError> {
    let encoding = match headers.get(axum::http::header::CONTENT_ENCODING) {
        None => String::new(),
        Some(value) => match value.to_str() {
            Ok(raw) => raw.trim().to_ascii_lowercase(),
            Err(_) => {
                return Err(RequestBodyError::UnsupportedEncoding("invalid".into()));
            }
        },
    };
    if !content_encoding_is_identity(&encoding) {
        // Reject before any decoder is constructed. gzip/br/deflate/zstd
        // (and anything else) are all 415; there is no accept branch.
        let label = if encoding.is_empty() {
            "invalid".into()
        } else {
            encoding
        };
        return Err(RequestBodyError::UnsupportedEncoding(label));
    }
    if body.len() <= max_bytes {
        Ok(body.to_vec())
    } else {
        Err(RequestBodyError::DecodedTooLarge)
    }
}

fn content_encoding_is_identity(encoding: &str) -> bool {
    encoding
        .split(',')
        .map(str::trim)
        .all(|part| part.is_empty() || part == "identity")
}

/// A model catalog entry as it appears on the wire for `GET /v1/models`.
/// Deliberately narrower than either side's real route type (Desktop's
/// `ModelRoute`, the daemon's `ModelRouteView`, or `RuntimeModelRoute`) —
/// this only carries what the response body actually needs, so both sides
/// can map their own type into it without adopting a shared route type
/// before M3 decides what that should be.
pub struct ModelListEntry {
    pub catalog_id: String,
    pub owned_by: String,
    pub context_window: Option<u64>,
}

pub fn models_list_payload(models: Vec<ModelListEntry>) -> Value {
    json!({
        "object": "list",
        "data": models.into_iter().map(|model| json!({
            "id": model.catalog_id,
            "object": "model",
            "owned_by": model.owned_by,
            "context_window": model.context_window,
        })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_encoding(encoding: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !encoding.is_empty() {
            headers.insert(
                axum::http::header::CONTENT_ENCODING,
                encoding.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn identity_body_within_limit_passes_through() {
        let headers = HeaderMap::new();
        let body = b"{\"hello\":\"world\"}";
        let decoded =
            decode_request_body_with_limit(&headers, body, MAX_REQUEST_BODY_BYTES).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn identity_body_over_limit_is_decoded_too_large() {
        let headers = HeaderMap::new();
        let body = vec![0u8; 10];
        let error = decode_request_body_with_limit(&headers, &body, 5).unwrap_err();
        assert!(matches!(error, RequestBodyError::DecodedTooLarge));
    }

    #[test]
    fn gzip_br_deflate_and_zstd_are_rejected_before_decompress() {
        for encoding in ["gzip", "br", "deflate", "zstd", "x-gzip", "gzip, br"] {
            let headers = headers_with_encoding(encoding);
            let error =
                decode_request_body_with_limit(&headers, b"anything", MAX_REQUEST_BODY_BYTES)
                    .unwrap_err();
            assert!(
                matches!(error, RequestBodyError::UnsupportedEncoding(ref label) if !label.is_empty()),
                "{encoding} must be refused without decoding: {error:?}"
            );
        }
    }

    #[test]
    fn identity_still_works_and_explicit_identity_is_accepted() {
        let headers = headers_with_encoding("identity");
        let body = b"{\"ok\":true}";
        let decoded =
            decode_request_body_with_limit(&headers, body, MAX_REQUEST_BODY_BYTES).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn oversize_identity_is_still_decoded_too_large() {
        let headers = headers_with_encoding("identity");
        let body = vec![0u8; 10];
        let error = decode_request_body_with_limit(&headers, &body, 5).unwrap_err();
        assert!(matches!(error, RequestBodyError::DecodedTooLarge));
    }

    #[tokio::test]
    async fn unsupported_encoding_answers_415_with_canonical_envelope() {
        let response = request_body_error_response(
            "/responses",
            RequestBodyError::UnsupportedEncoding("zstd".into()),
        );
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["type"], "vellum_proxy_error");
        assert_eq!(payload["error"]["category"], "unsupported_media");
        assert_eq!(payload["error"]["code"], 415);
    }

    #[test]
    fn invalid_json_after_decode_names_the_body_as_invalid() {
        let headers = HeaderMap::new();
        let error = parse_json_request(&headers, b"not json").unwrap_err();
        assert!(
            matches!(error, RequestBodyError::Invalid(ref message) if message.contains("Invalid JSON"))
        );
    }

    #[test]
    fn empty_body_gets_the_specific_empty_body_message() {
        let headers = HeaderMap::new();
        let error = parse_json_request(&headers, b"").unwrap_err();
        assert!(
            matches!(error, RequestBodyError::Invalid(ref message) if message.contains("empty HTTP request body"))
        );
    }

    #[test]
    fn models_list_payload_shape_has_exactly_the_wire_fields() {
        let payload = models_list_payload(vec![ModelListEntry {
            catalog_id: "vlm-1".into(),
            owned_by: "route-1".into(),
            context_window: Some(128_000),
        }]);
        assert_eq!(
            payload,
            json!({
                "object": "list",
                "data": [{
                    "id": "vlm-1",
                    "object": "model",
                    "owned_by": "route-1",
                    "context_window": 128_000,
                }]
            })
        );
    }
}
