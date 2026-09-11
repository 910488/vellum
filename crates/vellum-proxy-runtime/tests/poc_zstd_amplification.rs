//! Regression (V-12): inbound compression is refused before any decoder
//! runs. The original 9.6 KiB → 300 MiB zstd amplification PoC dies at
//! HTTP 415 / `unsupported_media` and never allocates the decoded body.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use vellum_proxy_runtime::body::{
    decode_request_body, request_body_error_response, RequestBodyError, MAX_REQUEST_BODY_BYTES,
};
use vellum_proxy_runtime::config::ProxyRuntimeConfig;
use vellum_proxy_runtime::server::build_headless_router;
use vellum_proxy_runtime::state::StaticProxyState;
use vellum_proxy_runtime::{
    BoundaryKey, InboundAccessPolicy, BOUNDARY_CREDENTIAL_ID, BOUNDARY_KEY_HEADER,
};

const BOUNDARY_KEY: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

#[test]
fn poc_zstd_body_is_rejected_before_decompress() {
    // A real zstd frame that *would* expand if a decoder ran. We never
    // allocate the 300 MiB identity payload: the encoding is refused first.
    let compressed = zstd::stream::encode_all(&b"amplification-canary"[..], 3).expect("compress");
    assert!(
        compressed.len() < MAX_REQUEST_BODY_BYTES,
        "the wire payload must stay far below the identity cap"
    );

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_ENCODING,
        axum::http::HeaderValue::from_static("zstd"),
    );

    let error = decode_request_body(&headers, &compressed).expect_err("zstd must not decode");
    assert!(
        matches!(error, RequestBodyError::UnsupportedEncoding(ref encoding) if encoding.contains("zstd")),
        "decode must fail closed on the encoding, got {error:?}"
    );

    let response = request_body_error_response("/responses", error);
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn poc_zstd_amplification_dies_at_415_on_the_shipped_router() {
    let compressed = zstd::stream::encode_all(&[0u8; 64 * 1024][..], 19).expect("compress");
    // Historical ratio: a few KiB of zeros at zstd-19 expands to a huge
    // identity body. Whatever the ratio, production inbound never decompresses.
    assert!(
        compressed.len() < 32 * 1024,
        "the PoC wire payload should stay small, got {}",
        compressed.len()
    );

    let temp = tempfile::tempdir().expect("tempdir for durable state");
    let mut config = ProxyRuntimeConfig {
        data_dir: temp.path().join("data"),
        history_dir: temp.path().join("history"),
        log_dir: temp.path().join("logs"),
        ..ProxyRuntimeConfig::default()
    };
    config
        .models
        .push(vellum_proxy_runtime::config::RuntimeRouteConfig {
            route_id: "mock".into(),
            catalog_id: "vellum-mock".into(),
            name: "Vellum Mock".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            provider_kind: vellum_proxy_runtime::RuntimeProviderKind::OpenAiCompatible,
            auth_kind: vellum_proxy_runtime::RuntimeAuthKind::None,
            wire: vellum_proxy_runtime::RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: "mock-1".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        });
    let state = StaticProxyState::from_config(config).unwrap();
    let policy = InboundAccessPolicy::authenticated(
        BOUNDARY_CREDENTIAL_ID,
        BoundaryKey::parse(BOUNDARY_KEY).expect("valid key"),
        15721,
    );
    let app = build_headless_router(Arc::new(state), policy);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:15721")
                .header(BOUNDARY_KEY_HEADER, BOUNDARY_KEY)
                .header("content-type", "application/json")
                .header("content-encoding", "zstd")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["error"]["type"], "vellum_proxy_error");
    assert_eq!(payload["error"]["category"], "unsupported_media");
    assert_eq!(payload["error"]["code"], 415);
}
