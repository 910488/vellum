//! Stop must cancel in-flight work on the real serve path without a 30s drain.

use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use vellum_proxy_runtime::config::ProxyRuntimeConfig;
use vellum_proxy_runtime::state::StaticProxyState;
use vellum_proxy_runtime::transport::{
    TransportError, UpstreamRequest, UpstreamResponse, UpstreamStream,
};
use vellum_proxy_runtime::{
    serve_proxy, BoundaryKey, InboundAccessPolicy, ProxyRuntimeState, BOUNDARY_CREDENTIAL_ID,
    BOUNDARY_KEY_HEADER,
};

const BOUNDARY_KEY: &str = "5f3a9c1e7b2d48a06e91c4f583b7d20e1a6c9f4b8d3e70a25c81f96b4d0e7a3c";

struct HangForever;

#[async_trait]
impl vellum_proxy_runtime::transport::UpstreamTransport for HangForever {
    async fn execute(
        &self,
        _request: &UpstreamRequest,
    ) -> Result<UpstreamResponse, TransportError> {
        std::future::pending::<()>().await;
        unreachable!()
    }

    async fn execute_streaming(
        &self,
        _request: &UpstreamRequest,
    ) -> Result<UpstreamStream, TransportError> {
        std::future::pending::<()>().await;
        unreachable!()
    }
}

fn hang_config(credentials_dir: &std::path::Path, temp: &std::path::Path) -> ProxyRuntimeConfig {
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
install_id = "stop-cancel"
host_id = "stop-cancel-host"
image_version = "0.0.0"
config_hash = "stop-cancel"

[[models]]
route_id = "hang-route"
catalog_id = "hang-model"
name = "Hang"
base_url = "http://127.0.0.1:1/v1"
provider_kind = "openAiCompatible"
auth_kind = "none"
wire = "responses"
server_side_resume = false
streaming = true
reasoning = false
vision = false
upstream_model = "hang-upstream"
context_window = 128000

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

async fn start_hanging_proxy() -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    Arc<StaticProxyState>,
    tempfile::TempDir,
) {
    let temp = tempfile::tempdir().expect("tempdir");
    let credentials_dir = temp.path().join("credentials");
    std::fs::create_dir_all(&credentials_dir).unwrap();
    let mut secret = std::fs::File::create(credentials_dir.join("unused")).unwrap();
    secret.write_all(b"x").unwrap();
    drop(secret);

    let state = Arc::new(
        StaticProxyState::from_config_with_transport(
            hang_config(&credentials_dir, temp.path()),
            Arc::new(HangForever),
        )
        .expect("state"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().unwrap();
    let policy = InboundAccessPolicy::authenticated(
        BOUNDARY_CREDENTIAL_ID,
        BoundaryKey::parse(BOUNDARY_KEY).unwrap(),
        address.port(),
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = serve_proxy(serve_state, listener, policy, shutdown_rx).await;
    });
    (address, shutdown_tx, state, temp)
}

async fn post_hanging_turn(
    address: std::net::SocketAddr,
    stream: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    client
        .post(format!("http://{address}/v1/responses"))
        .header(BOUNDARY_KEY_HEADER, BOUNDARY_KEY)
        .json(&serde_json::json!({
            "model": "hang-model",
            "input": [{"role": "user", "content": "ping"}],
            "stream": stream
        }))
        .send()
        .await
}

async fn one_stop_cancels_inflight(stream: bool) {
    let (address, shutdown_tx, state, _temp) = start_hanging_proxy().await;
    let pending = tokio::spawn(async move { post_hanging_turn(address, stream).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let started = Instant::now();
    let _ = shutdown_tx.send(());
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending).await;
    let elapsed = started.elapsed();
    eprintln!(
        "stop-cancel inner stream={stream} elapsed_ms={} records={}",
        elapsed.as_millis(),
        state
            .proxy_runtime()
            .usage_records()
            .map(|rows| rows.len())
            .unwrap_or(0)
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "stop waited {elapsed:?}; must not drain for ~30s"
    );
    match outcome {
        Ok(Ok(Ok(response))) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            assert!(
                !body.contains("\"status\":\"completed\""),
                "interrupted work must not be recorded as success: {status} {body}"
            );
            assert!(
                !body.contains("provider_error") && !body.contains("\"category\":\"provider"),
                "must not classify stop as a provider error: {body}"
            );
            if stream {
                // SSE opens with HTTP 200, then a failed event. That is not a
                // successful turn.
                assert!(
                    body.contains("proxy_stopped") || body.contains("user stopped proxy"),
                    "streaming stop must surface proxy_stopped: {body}"
                );
            } else {
                assert!(
                    !status.is_success(),
                    "interrupted work must not be recorded as success: {status} {body}"
                );
            }
        }
        Ok(Ok(Err(_))) | Ok(Err(_)) => {
            // Connection dropped by immediate serve abort — still not success.
        }
        Err(_) => panic!("in-flight request did not end after stop"),
    }
    let records = state.proxy_runtime().usage_records().unwrap_or_default();
    if !records.is_empty() {
        assert!(
            records
                .iter()
                .all(|row| row.outcome.as_deref() != Some("success")),
            "stop must not persist a success outcome: {records:?}"
        );
        assert!(
            records.iter().any(|row| {
                row.outcome.as_deref() == Some("user_stopped_proxy")
                    || row.error_category.as_deref() == Some("proxy_stopped")
            }),
            "expected user_stopped_proxy usage row: {records:?}"
        );
    }

    let second = tokio::time::timeout(
        Duration::from_millis(400),
        post_hanging_turn(address, stream),
    )
    .await;
    if let Ok(Ok(response)) = second {
        assert!(
            !response.status().is_success(),
            "new requests after stop must be refused"
        );
    }
}

async fn one_stop_cancels_websocket() {
    let (address, shutdown_tx, state, _temp) = start_hanging_proxy().await;
    let mut request = format!("ws://{address}/v1/responses")
        .into_client_request()
        .expect("websocket request");
    request.headers_mut().insert(
        BOUNDARY_KEY_HEADER,
        BOUNDARY_KEY.parse().expect("header value"),
    );
    let (mut socket, _response) = connect_async(request).await.expect("websocket upgrade");
    let create = serde_json::json!({
        "type": "response.create",
        "model": "hang-model",
        "input": [{"role": "user", "content": "ping"}],
        "stream": true
    });
    socket
        .send(Message::Text(create.to_string().into()))
        .await
        .expect("send hanging create");
    tokio::time::sleep(Duration::from_millis(80)).await;
    let started = Instant::now();
    let _ = shutdown_tx.send(());
    let outcome = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
    let elapsed = started.elapsed();
    eprintln!(
        "stop-cancel inner websocket elapsed_ms={} records={}",
        elapsed.as_millis(),
        state
            .proxy_runtime()
            .usage_records()
            .map(|rows| rows.len())
            .unwrap_or(0)
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "websocket stop waited {elapsed:?}; must not drain for ~30s"
    );
    match outcome {
        Ok(Some(Ok(Message::Text(text)))) => {
            assert!(
                !text.contains("\"status\":\"completed\""),
                "interrupted websocket turn must not be success: {text}"
            );
            assert!(
                text.contains("proxy_stopped")
                    || text.contains("user stopped proxy")
                    || text.contains("failed"),
                "expected proxy_stopped on websocket: {text}"
            );
        }
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(_))) => {}
        Err(_) => panic!("in-flight websocket did not end after stop"),
    }
}

#[tokio::test]
async fn stop_cancels_inflight_without_thirty_second_drain() {
    one_stop_cancels_inflight(false).await;
    one_stop_cancels_inflight(false).await;
    one_stop_cancels_inflight(true).await;
    one_stop_cancels_inflight(true).await;
    one_stop_cancels_websocket().await;
    one_stop_cancels_websocket().await;
}
