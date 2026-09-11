//! Regression: the local proxy boundary (V-01/V-02).
//!
//! Originally a Cross-Site WebSocket Hijacking POC. `build_headless_router`
//! installed no CORS layer, no `Origin` check and no caller authentication.
//! WebSocket upgrades are *not* subject to the same-origin policy — the browser
//! sends `Origin` and expects the server to reject foreign ones. Vellum never
//! read it, so any page the user visited could open a socket to
//! `127.0.0.1:15721` and drive the user's configured routes with the user's
//! stored credential.
//!
//! The handshakes below go over a raw TCP socket, with no ws client dependency,
//! so the exact bytes a browser would send stay visible.

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vellum_proxy_runtime::config::ProxyRuntimeConfig;
use vellum_proxy_runtime::server::build_headless_router;
use vellum_proxy_runtime::state::StaticProxyState;
use vellum_proxy_runtime::transport::{
    TransportError, UpstreamRequest, UpstreamResponse, UpstreamTransport,
};
use vellum_proxy_runtime::{
    BoundaryKey, InboundAccessPolicy, BOUNDARY_CREDENTIAL_ID, BOUNDARY_KEY_HEADER,
};

const VICTIM_SECRET: &str = "sk-victim-BILLED-TO-THE-USER";
const ATTACKER_ORIGIN: &str = "https://evil.example";
const BOUNDARY_KEY: &str = "5f3a9c1e7b2d48a06e91c4f583b7d20e1a6c9f4b8d3e70a25c81f96b4d0e7a3c";

/// Records what the runtime would have sent upstream. Stands in for the real
/// provider so this test never touches the network.
#[derive(Default)]
struct RecordingTransport {
    seen: Mutex<Vec<UpstreamRequest>>,
}

#[async_trait]
impl UpstreamTransport for RecordingTransport {
    async fn execute(&self, request: &UpstreamRequest) -> Result<UpstreamResponse, TransportError> {
        self.seen.lock().unwrap().push(request.clone());
        Ok(UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&serde_json::json!({
                "id": "chatcmpl-regression",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "EXFIL-MARKER"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
            .unwrap(),
        })
    }
}

fn victim_config(credentials_dir: &std::path::Path, temp: &std::path::Path) -> ProxyRuntimeConfig {
    let raw = format!(
        r#"
schema_version = 2
listen = "127.0.0.1:0"
data_dir = {data:?}
history_dir = {history:?}
log_dir = {logs:?}
credentials_dir = {creds:?}
require_secrets = false
strict_upstream = false

[inbound_access]
credentialId = "{boundary}"

[identity]
install_id = "regression-install"
host_id = "regression-host"
image_version = "0.0.0"
config_hash = "regression"

[[models]]
route_id = "victim-route"
catalog_id = "victim-model"
name = "Victim Route"
base_url = "http://127.0.0.1:1/v1"
provider_kind = "openAiCompatible"
auth_kind = "bearer"
wire = "chat"
server_side_resume = false
streaming = false
reasoning = false
vision = false
upstream_model = "victim-upstream"
context_window = 128000
credential_id = "victim-credential"

[execution_environment]
platform = "linux"
shell = "bash"
supportsAndAnd = true
hasUnixUtilities = true
pathStyle = "posix"
ampersandSemantics = "posix-background"
"#,
        data = temp.join("data"),
        history = temp.join("history"),
        logs = temp.join("logs"),
        creds = credentials_dir,
        boundary = BOUNDARY_CREDENTIAL_ID,
    );
    ProxyRuntimeConfig::from_toml_str(&raw).expect("config must load")
}

/// Encode one masked client->server text frame, exactly as a browser does.
fn masked_text_frame(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let mut frame = vec![0x81u8]; // FIN + opcode text
    let mask_bit = 0x80u8;
    match bytes.len() {
        len if len < 126 => frame.push(mask_bit | len as u8),
        len if len < 65536 => {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    frame.extend_from_slice(&mask);
    for (index, byte) in bytes.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

struct Harness {
    address: std::net::SocketAddr,
    transport: Arc<RecordingTransport>,
    _temp: tempfile::TempDir,
}

/// Start a proxy with the production boundary policy in front of it.
async fn start_guarded_proxy() -> Harness {
    let temp = tempfile::tempdir().expect("tempdir");
    let credentials_dir = temp.path().join("credentials");
    std::fs::create_dir_all(&credentials_dir).expect("credentials dir");
    let mut secret_file =
        std::fs::File::create(credentials_dir.join("victim-credential")).expect("secret file");
    secret_file
        .write_all(VICTIM_SECRET.as_bytes())
        .expect("write secret");
    drop(secret_file);

    let transport = Arc::new(RecordingTransport::default());
    let state = StaticProxyState::from_config_with_transport(
        victim_config(&credentials_dir, temp.path()),
        transport.clone(),
    )
    .expect("state builds");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("local addr");
    let policy = InboundAccessPolicy::authenticated(
        BOUNDARY_CREDENTIAL_ID,
        BoundaryKey::parse(BOUNDARY_KEY).expect("valid key"),
        address.port(),
    );
    let router = build_headless_router(Arc::new(state), policy);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Harness {
        address,
        transport,
        _temp: temp,
    }
}

/// Perform a WebSocket handshake with the given extra headers and return the
/// socket plus the server's status line.
async fn websocket_handshake(
    address: std::net::SocketAddr,
    extra_headers: &str,
) -> (tokio::net::TcpStream, String) {
    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to the local proxy");
    let handshake = format!(
        "GET /v1/responses HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {extra_headers}\
         \r\n",
        port = address.port(),
    );
    socket
        .write_all(handshake.as_bytes())
        .await
        .expect("send handshake");
    let mut buffer = [0u8; 1024];
    let read = socket.read(&mut buffer).await.expect("read handshake");
    let response = String::from_utf8_lossy(&buffer[..read]).to_string();
    let status = response.lines().next().unwrap_or_default().to_string();
    (socket, status)
}

/// The original attack: a page the user visits opens a socket to the proxy and
/// drives the user's routes. It carries an `Origin`, which no Codex client
/// sends, and it cannot read the boundary key.
#[tokio::test]
async fn a_cross_origin_websocket_is_refused_before_it_reaches_a_route() {
    let harness = start_guarded_proxy().await;
    let (_socket, status) =
        websocket_handshake(harness.address, &format!("Origin: {ATTACKER_ORIGIN}\r\n")).await;

    assert!(
        status.contains("400"),
        "a foreign Origin must be refused, got: {status}"
    );
    assert!(
        !status.contains("101"),
        "the proxy must not upgrade the socket: {status}"
    );
    assert!(
        harness.transport.seen.lock().unwrap().is_empty(),
        "no upstream dispatch may happen for a rejected caller"
    );
}

/// Even without an `Origin`, a caller that cannot present the boundary key gets
/// nowhere — this is the foreign-local-process case, not the browser one.
#[tokio::test]
async fn a_websocket_without_the_boundary_key_is_refused() {
    let harness = start_guarded_proxy().await;
    let (_socket, status) = websocket_handshake(harness.address, "").await;
    assert!(
        status.contains("401"),
        "a missing boundary key must be an authentication failure, got: {status}"
    );
    assert!(harness.transport.seen.lock().unwrap().is_empty());
}

/// The legitimate caller still works end to end, and the key it presented never
/// travels upstream.
#[tokio::test]
async fn an_authenticated_websocket_still_drives_the_route() {
    let harness = start_guarded_proxy().await;
    let (mut socket, status) = websocket_handshake(
        harness.address,
        &format!("{BOUNDARY_KEY_HEADER}: {BOUNDARY_KEY}\r\n"),
    )
    .await;
    assert!(
        status.contains("101"),
        "the authenticated caller must be upgraded, got: {status}"
    );

    let request = serde_json::json!({
        "model": "victim-model",
        "stream": false,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "legitimate prompt"}]
        }]
    })
    .to_string();
    socket
        .write_all(&masked_text_frame(&request))
        .await
        .expect("send request frame");

    let mut reply = Vec::new();
    let mut chunk = [0u8; 4096];
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut chunk)).await
        {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(count)) => {
                reply.extend_from_slice(&chunk[..count]);
                if String::from_utf8_lossy(&reply).contains("EXFIL-MARKER") {
                    break;
                }
            }
            Ok(Err(_)) => break,
        }
    }
    let reply = String::from_utf8_lossy(&reply).to_string();
    assert!(
        reply.contains("EXFIL-MARKER"),
        "the authenticated caller must get its reply: {reply}"
    );

    let upstream = harness.transport.seen.lock().unwrap();
    let sent = upstream.first().expect("the route must have dispatched");
    assert!(
        sent.headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case(BOUNDARY_KEY_HEADER)),
        "the boundary key authenticates the caller to Vellum and must never \
         reach a provider: {:?}",
        sent.headers
    );
    assert!(
        sent.headers
            .iter()
            .all(|(_, value)| !value.contains(BOUNDARY_KEY)),
        "the boundary key must not appear in any upstream header value"
    );
    // The route's own credential is still applied, so this proves the guard
    // did not simply break authentication.
    assert!(sent
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
            && value.contains(VICTIM_SECRET)));
}

/// Every endpoint is behind the guard, including the ones that look harmless.
/// An unauthenticated `/health` is still a probe that finds the proxy and
/// fingerprints the install.
#[tokio::test]
async fn every_endpoint_requires_the_boundary_key() {
    let harness = start_guarded_proxy().await;
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}", harness.address.port());

    for path in [
        "/health",
        "/readyz",
        "/version",
        "/diagnostics/usage",
        "/v1/models",
    ] {
        let response = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            response.status().as_u16(),
            401,
            "{path} must require the boundary key"
        );
        let body: serde_json::Value = response.json().await.expect("canonical envelope");
        assert_eq!(
            body["error"]["category"], "authentication_failed",
            "{path} must use the canonical category"
        );

        let response = client
            .get(format!("{base}{path}"))
            .header(BOUNDARY_KEY_HEADER, BOUNDARY_KEY)
            .send()
            .await
            .expect("request");
        assert!(
            response.status().is_success() || response.status().as_u16() == 503,
            "{path} must answer an authenticated caller, got {}",
            response.status()
        );
    }
}

/// A wrong key is an authentication failure, and an `Origin` on a plain HTTP
/// POST is refused the same way the upgrade is.
#[tokio::test]
async fn http_posts_are_guarded_on_the_same_terms_as_the_upgrade() {
    let harness = start_guarded_proxy().await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/v1/responses", harness.address.port());
    let body = serde_json::json!({
        "model": "victim-model",
        "stream": false,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "attacker-controlled prompt"}]
        }]
    });

    let wrong_key = client
        .post(&url)
        .header(BOUNDARY_KEY_HEADER, "0".repeat(64))
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(wrong_key.status().as_u16(), 401);

    let with_origin = client
        .post(&url)
        .header("origin", ATTACKER_ORIGIN)
        .header(BOUNDARY_KEY_HEADER, BOUNDARY_KEY)
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(
        with_origin.status().as_u16(),
        400,
        "an Origin header is refused even alongside a valid key"
    );

    assert!(
        harness.transport.seen.lock().unwrap().is_empty(),
        "neither rejected request may reach the upstream"
    );
}

/// A request addressed to something other than this proxy's own loopback host
/// is refused, which is what closes DNS rebinding: the attacker's name resolves
/// to 127.0.0.1, but the `Host` it sends is still their domain.
#[tokio::test]
async fn a_foreign_host_header_is_refused() {
    let harness = start_guarded_proxy().await;
    let mut socket = tokio::net::TcpStream::connect(harness.address)
        .await
        .expect("connect");
    let request = format!(
        "GET /health HTTP/1.1\r\n\
         Host: attacker.example\r\n\
         {BOUNDARY_KEY_HEADER}: {BOUNDARY_KEY}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut response = String::new();
    let mut buffer = [0u8; 4096];
    while let Ok(count) = socket.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        response.push_str(&String::from_utf8_lossy(&buffer[..count]));
        if response.contains("\r\n\r\n") {
            break;
        }
    }
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "a foreign Host must be refused even with a valid key: {response}"
    );
}
