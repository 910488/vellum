//! Codex Desktop's own ChatGPT backend traffic, relayed through the proxy.
//!
//! Desktop's main process sends every renderer backend request (`/wham/*`,
//! `/accounts/*`, ...) to `CODEX_API_BASE_URL` when that variable is set, and
//! attaches the signed-in account's token to it. Vellum points that variable
//! here so it can answer one question differently: Desktop disables its
//! composer when `/wham/usage` says the *signed-in* account is out of quota,
//! even though the turn itself goes through this proxy and is served by
//! whichever account Vellum routes to. Usage responses are therefore reported
//! as unlimited; everything else is forwarded unchanged, under Desktop's own
//! identity, so the phone and Desktop stay on the same account.
//!
//! The relay adds no Vellum credential to anything it forwards, which is why
//! it needs no boundary key (Desktop cannot send one). Host and Origin checks
//! still apply.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;

/// Path prefix of the relay. Desktop's base URL is
/// `http://127.0.0.1:<port>/desktop-backend/backend-api`.
pub const DESKTOP_BACKEND_PREFIX: &str = "/desktop-backend/";
pub const DESKTOP_BACKEND_BASE_PATH: &str = "/desktop-backend/backend-api";

/// Desktop rewrites its update feed to this path whenever the backend base URL
/// is a loopback host.
pub const DESKTOP_APPCAST_PATH: &str = "/api/codex/app/appcast";

const PRODUCTION_BACKEND: &str = "https://chatgpt.com/backend-api";
const PRODUCTION_APPCAST: &str = "https://updates.oaistatic.com/codex/app/appcast";
/// Desktop's backend request bodies are small JSON; this only bounds memory.
const MAX_RELAY_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The value Vellum leases into `CODEX_API_BASE_URL`.
pub fn desktop_backend_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}{DESKTOP_BACKEND_BASE_PATH}")
}

/// Whether a request is for the keyless relay surface.
pub fn is_keyless_desktop_route(method: &Method, path: &str) -> bool {
    path.starts_with(DESKTOP_BACKEND_PREFIX)
        || (method == Method::GET && path == DESKTOP_APPCAST_PATH)
}

#[derive(Clone)]
pub struct DesktopBackendRelay {
    backend: String,
    appcast: String,
    client: reqwest::Client,
}

impl DesktopBackendRelay {
    pub fn production() -> Self {
        Self::with_upstreams(PRODUCTION_BACKEND, PRODUCTION_APPCAST)
    }

    pub fn with_upstreams(backend: impl Into<String>, appcast: impl Into<String>) -> Self {
        Self {
            backend: backend.into().trim_end_matches('/').to_string(),
            appcast: appcast.into(),
            client: reqwest::Client::builder()
                // Desktop sends these with `redirect: "error"`; following one
                // here would hide it.
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }
}

pub fn router(relay: Arc<DesktopBackendRelay>) -> Router {
    Router::new()
        .route("/desktop-backend/backend-api/{*rest}", any(relay_backend))
        .route(DESKTOP_APPCAST_PATH, get(relay_appcast))
        .with_state(relay)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageShape {
    Json,
    EventStream,
}

fn usage_shape(rest: &str) -> Option<UsageShape> {
    match rest.trim_end_matches('/') {
        "wham/usage" => Some(UsageShape::Json),
        "wham/usage/stream" => Some(UsageShape::EventStream),
        _ => None,
    }
}

async fn relay_backend(
    State(relay): State<Arc<DesktopBackendRelay>>,
    Path(rest): Path<String>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let mut url = format!("{}/{}", relay.backend, rest);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    if is_websocket_upgrade(&parts.headers) {
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return rejection.into_response(),
        };
        return relay_websocket(upgrade, &url, &parts.headers).await;
    }
    // Buffered so the upstream sees a Content-Length (and none on a GET)
    // rather than a chunked body Desktop never sent.
    let body = match axum::body::to_bytes(body, MAX_RELAY_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "desktop backend relay: body too large",
            )
                .into_response()
        }
    };
    let usage = usage_shape(&rest);
    let mut headers = forwardable_headers(&parts.headers);
    if usage.is_some() {
        // The body is rewritten, so it has to arrive uncompressed.
        headers.remove(axum::http::header::ACCEPT_ENCODING);
    }
    let upstream = relay
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(body)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => return bad_gateway(&error),
    };
    match usage {
        Some(shape) if upstream.status().is_success() => unlimited_usage(upstream, shape).await,
        _ => passthrough(upstream),
    }
}

async fn relay_appcast(
    State(relay): State<Arc<DesktopBackendRelay>>,
    request: Request,
) -> Response {
    let mut url = relay.appcast.clone();
    if let Some(query) = request.uri().query() {
        url.push('?');
        url.push_str(query);
    }
    // Public feed: nothing from the caller is forwarded but what it accepts.
    let mut builder = relay.client.get(url);
    if let Some(accept) = request.headers().get(axum::http::header::ACCEPT) {
        builder = builder.header(axum::http::header::ACCEPT, accept.clone());
    }
    match builder.send().await {
        Ok(response) => passthrough(response),
        Err(error) => bad_gateway(&error),
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// Desktop's dictation stream is a WebSocket built from the same base URL.
/// The upstream handshake happens first, so a refusal reaches Desktop as an
/// HTTP error instead of a socket that opens and immediately dies.
async fn relay_websocket(upgrade: WebSocketUpgrade, url: &str, inbound: &HeaderMap) -> Response {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = match url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => url.to_string(),
    };
    let mut request = match url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            log::warn!("[DesktopBackend] invalid WebSocket URL: {error}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    for (name, value) in inbound {
        let handshake = name.as_str().starts_with("sec-websocket-")
            && name != axum::http::header::SEC_WEBSOCKET_PROTOCOL;
        if handshake
            || is_hop_by_hop(name)
            || name == axum::http::header::HOST
            || name == axum::http::header::CONTENT_LENGTH
            || name.as_str() == crate::inbound::BOUNDARY_KEY_HEADER
        {
            continue;
        }
        request.headers_mut().append(name.clone(), value.clone());
    }
    let (upstream, response) =
        match tokio_tungstenite::connect_async_with_config(request, None, true).await {
            Ok(connected) => connected,
            Err(tokio_tungstenite::tungstenite::Error::Http(refused)) => {
                let status = StatusCode::from_u16(refused.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                return status.into_response();
            }
            Err(error) => {
                log::warn!("[DesktopBackend] WebSocket upstream failed: {error}");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
    let upgrade = match response
        .headers()
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
    {
        Some(protocol) => upgrade.protocols([protocol.to_string()]),
        None => upgrade,
    };
    upgrade.on_upgrade(move |client| pump_websocket(client, upstream))
}

async fn pump_websocket(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame as TungsteniteClose;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let outbound = async {
        while let Some(Ok(message)) = client_rx.next().await {
            let message = match message {
                AxumWsMessage::Text(text) => TungsteniteMessage::Text(text.as_str().into()),
                AxumWsMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
                AxumWsMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
                AxumWsMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
                AxumWsMessage::Close(frame) => {
                    let frame = frame.map(|frame| TungsteniteClose {
                        code: CloseCode::from(frame.code),
                        reason: frame.reason.as_str().into(),
                    });
                    let _ = upstream_tx.send(TungsteniteMessage::Close(frame)).await;
                    break;
                }
            };
            if upstream_tx.send(message).await.is_err() {
                break;
            }
        }
    };
    let inbound = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let message = match message {
                TungsteniteMessage::Text(text) => AxumWsMessage::Text(text.as_str().into()),
                TungsteniteMessage::Binary(bytes) => AxumWsMessage::Binary(bytes),
                TungsteniteMessage::Ping(bytes) => AxumWsMessage::Ping(bytes),
                TungsteniteMessage::Pong(bytes) => AxumWsMessage::Pong(bytes),
                TungsteniteMessage::Close(frame) => {
                    let frame = frame.map(|frame| axum::extract::ws::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.as_str().into(),
                    });
                    let _ = client_tx.send(AxumWsMessage::Close(frame)).await;
                    break;
                }
                TungsteniteMessage::Frame(_) => continue,
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
    };
    // Either side ending ends the relay; dropping the other half closes it.
    tokio::select! {
        _ = outbound => {}
        _ = inbound => {}
    }
}

fn bad_gateway(error: &reqwest::Error) -> Response {
    log::warn!("[DesktopBackend] upstream request failed: {error}");
    (
        StatusCode::BAD_GATEWAY,
        "desktop backend relay: upstream unavailable",
    )
        .into_response()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn forwardable_headers(inbound: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in inbound {
        if is_hop_by_hop(name)
            || name == axum::http::header::HOST
            || name == axum::http::header::CONTENT_LENGTH
            || name.as_str() == crate::inbound::BOUNDARY_KEY_HEADER
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

fn response_headers(upstream: &reqwest::header::HeaderMap, drop_body_framing: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name)
            || (drop_body_framing
                && (name == axum::http::header::CONTENT_LENGTH
                    || name == axum::http::header::CONTENT_ENCODING))
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

fn passthrough(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = response_headers(upstream.headers(), false);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn unlimited_usage(upstream: reqwest::Response, shape: UsageShape) -> Response {
    let status = upstream.status();
    let headers = response_headers(upstream.headers(), true);
    let body = match shape {
        UsageShape::Json => match upstream.bytes().await {
            Ok(bytes) => Body::from(rewrite_usage_json(&bytes)),
            Err(error) => return bad_gateway(&error),
        },
        UsageShape::EventStream => Body::from_stream(rewrite_event_stream(upstream.bytes_stream())),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn rewrite_usage_json(bytes: &[u8]) -> Bytes {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(mut value) => {
            mark_unlimited(&mut value);
            Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec()))
        }
        // Not JSON: hand it back as it came rather than invent a body.
        Err(_) => Bytes::copy_from_slice(bytes),
    }
}

/// Rewrites each complete `data:` line of an event stream; partial lines wait
/// for the rest of their bytes.
fn rewrite_event_stream<S>(
    upstream: S,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    async_stream::stream! {
        let mut pending: Vec<u8> = Vec::new();
        futures_util::pin_mut!(upstream);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            pending.extend_from_slice(&chunk);
            if let Some(end) = pending.iter().rposition(|byte| *byte == b'\n') {
                let rest = pending.split_off(end + 1);
                let complete = std::mem::replace(&mut pending, rest);
                yield Ok(Bytes::from(rewrite_event_lines(&complete)));
            }
        }
        if !pending.is_empty() {
            yield Ok(Bytes::from(rewrite_event_lines(&pending)));
        }
    }
}

fn rewrite_event_lines(chunk: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(chunk);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, ending) = match line.strip_suffix("\r\n") {
            Some(content) => (content, "\r\n"),
            None => match line.strip_suffix('\n') {
                Some(content) => (content, "\n"),
                None => (line, ""),
            },
        };
        match content.strip_prefix("data:") {
            Some(data) => match serde_json::from_str::<Value>(data.trim_start()) {
                Ok(mut value) => {
                    mark_unlimited(&mut value);
                    out.push_str("data: ");
                    out.push_str(&value.to_string());
                }
                Err(_) => out.push_str(content),
            },
            None => out.push_str(content),
        }
        out.push_str(ending);
    }
    out.into_bytes()
}

/// Turns a `wham/usage` payload into one Desktop reads as "nothing reached".
///
/// Desktop blocks sending when any of these say so: `rate_limit_reached_type`
/// is set, a limit has `limit_reached` or not `allowed`, a spend control is
/// `reached`, or a plan has neither credits nor unlimited credits. The same
/// shape repeats under `additional_rate_limits`, hence the walk.
pub fn mark_unlimited(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, field) in object.iter_mut() {
                match key.as_str() {
                    "rate_limit_reached_type" => *field = Value::Null,
                    "limit_reached" | "reached" => *field = Value::Bool(false),
                    "allowed" => *field = Value::Bool(true),
                    "used_percent" => *field = Value::from(0),
                    "credits" => {
                        if let Value::Object(credits) = field {
                            credits.insert("has_credits".into(), Value::Bool(true));
                            credits.insert("unlimited".into(), Value::Bool(true));
                        }
                    }
                    _ => mark_unlimited(field),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mark_unlimited),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exhausted() -> Value {
        json!({
            "plan_type": "team",
            "rate_limit_reached_type": {"type": "workspace_owner_credits_depleted", "details": null},
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 100, "reset_after_seconds": 2548},
                "secondary_window": {"used_percent": 25, "reset_after_seconds": 535279}
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            "spend_control": {"reached": true},
            "additional_rate_limits": [
                {"limit_name": "gpt-6", "rate_limit": {"allowed": false, "limit_reached": true}}
            ]
        })
    }

    #[test]
    fn exhausted_usage_reads_as_unlimited() {
        let mut usage = exhausted();
        mark_unlimited(&mut usage);
        assert_eq!(usage["rate_limit_reached_type"], Value::Null);
        assert_eq!(usage["rate_limit"]["allowed"], true);
        assert_eq!(usage["rate_limit"]["limit_reached"], false);
        assert_eq!(usage["rate_limit"]["primary_window"]["used_percent"], 0);
        assert_eq!(
            usage["rate_limit"]["primary_window"]["reset_after_seconds"],
            2548
        );
        assert_eq!(usage["credits"]["has_credits"], true);
        assert_eq!(usage["credits"]["unlimited"], true);
        assert_eq!(usage["spend_control"]["reached"], false);
        assert_eq!(
            usage["additional_rate_limits"][0]["rate_limit"]["limit_reached"],
            false
        );
        assert_eq!(usage["plan_type"], "team");
    }

    #[test]
    fn event_stream_data_lines_are_rewritten_and_framing_kept() {
        let input = format!("event: usage\r\ndata: {}\r\n\r\n: keepalive\n", exhausted());
        let output = String::from_utf8(rewrite_event_lines(input.as_bytes())).unwrap();
        let lines = output.split("\r\n").collect::<Vec<_>>();
        assert_eq!(lines[0], "event: usage");
        let data: Value = serde_json::from_str(lines[1].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(data["rate_limit"]["limit_reached"], false);
        assert!(output.ends_with("\r\n\r\n: keepalive\n"));
    }

    #[tokio::test]
    async fn event_stream_waits_for_a_whole_line() {
        let line = format!("data: {}\n", json!({"rate_limit": {"limit_reached": true}}));
        let (head, tail) = line.split_at(10);
        let chunks = vec![
            Ok(Bytes::copy_from_slice(head.as_bytes())),
            Ok(Bytes::copy_from_slice(tail.as_bytes())),
        ];
        let output = rewrite_event_stream(futures_util::stream::iter(chunks))
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"limit_reached\":false"), "{output}");
    }

    #[tokio::test]
    async fn websocket_is_relayed_with_desktop_identity() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        async fn serve(app: Router) -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            address
        }

        let upstream = serve(Router::new().route(
            "/backend-api/dictation/stream",
            get(|upgrade: WebSocketUpgrade, headers: HeaderMap| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                upgrade.on_upgrade(move |mut socket| async move {
                    while let Some(Ok(AxumWsMessage::Text(text))) = socket.next().await {
                        let reply = format!("{auth}|{}", text.as_str());
                        if socket
                            .send(AxumWsMessage::Text(reply.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            }),
        ))
        .await;
        let relay = serve(router(Arc::new(DesktopBackendRelay::with_upstreams(
            format!("http://{upstream}/backend-api"),
            format!("http://{upstream}/appcast"),
        ))))
        .await;

        let mut request = format!("ws://{relay}/desktop-backend/backend-api/dictation/stream")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer desktop-login".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(TungsteniteMessage::Text("hello".into()))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap();
        assert_eq!(
            reply.into_text().unwrap().as_str(),
            "Bearer desktop-login|hello"
        );
    }

    #[test]
    fn only_usage_paths_are_rewritten() {
        assert!(usage_shape("wham/usage") == Some(UsageShape::Json));
        assert!(usage_shape("wham/usage/stream") == Some(UsageShape::EventStream));
        assert!(usage_shape("wham/usage/thread_usage/query").is_none());
        assert!(usage_shape("wham/tasks/list").is_none());
    }

    #[test]
    fn base_url_and_keyless_routes() {
        assert_eq!(
            desktop_backend_base_url(15721),
            "http://127.0.0.1:15721/desktop-backend/backend-api"
        );
        assert!(is_keyless_desktop_route(
            &Method::POST,
            "/desktop-backend/backend-api/wham/tasks"
        ));
        assert!(is_keyless_desktop_route(&Method::GET, DESKTOP_APPCAST_PATH));
        assert!(!is_keyless_desktop_route(
            &Method::POST,
            DESKTOP_APPCAST_PATH
        ));
        assert!(!is_keyless_desktop_route(&Method::POST, "/v1/responses"));
    }
}
