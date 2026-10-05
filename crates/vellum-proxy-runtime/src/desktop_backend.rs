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
//! it needs no boundary key (Desktop cannot send one).
//!
//! It listens on its own port, not the proxy's: Desktop attaches its login
//! only to OpenAI hosts and to exactly `localhost` or `localhost:8000`, so
//! the relay takes port 80 where the OS allows it and 8000 otherwise. Any
//! other loopback address makes every authenticated call fail before it is
//! sent, and Desktop reloads in a loop.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;

pub const DESKTOP_BACKEND_BASE_PATH: &str = "/desktop-backend/backend-api";

/// Desktop rewrites its update feed to this path whenever the backend base URL
/// is a loopback host.
pub const DESKTOP_APPCAST_PATH: &str = "/api/codex/app/appcast";

/// The only loopback ports Desktop will send its login to, in preference
/// order. macOS refuses port 80 to an unprivileged process.
pub const DESKTOP_RELAY_PORTS: [u16; 2] = [80, 8000];

const PRODUCTION_BACKEND: &str = "https://chatgpt.com/backend-api";
const PRODUCTION_APPCAST: &str = "https://updates.oaistatic.com/codex/app/appcast";
/// Desktop's backend request bodies are small JSON; this only bounds memory.
const MAX_RELAY_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The value Vellum leases into `CODEX_API_BASE_URL`.
pub fn desktop_backend_base_url(port: u16) -> String {
    match port {
        80 => format!("http://localhost{DESKTOP_BACKEND_BASE_PATH}"),
        port => format!("http://localhost:{port}{DESKTOP_BACKEND_BASE_PATH}"),
    }
}

/// The relay's listening sockets, bound but not yet serving.
pub struct DesktopRelayListener {
    port: u16,
    listeners: Vec<TcpListener>,
}

impl DesktopRelayListener {
    /// Binds the first usable port in [`DESKTOP_RELAY_PORTS`].
    ///
    /// `localhost` may resolve to either loopback family, so both are bound.
    /// IPv4 is required; IPv6 is skipped only when the host has no IPv6
    /// loopback, never when something else already owns that port there —
    /// half of Desktop's requests would reach the other program.
    pub async fn bind() -> Option<Self> {
        for port in DESKTOP_RELAY_PORTS {
            match Self::bind_port(port).await {
                Ok(listener) => return Some(listener),
                Err(error) => {
                    log::info!("[DesktopBackend] localhost:{port} unavailable: {error}")
                }
            }
        }
        None
    }

    async fn bind_port(port: u16) -> std::io::Result<Self> {
        // Windows and BSD let a loopback bind succeed beside another program's
        // wildcard listener, which would quietly take its localhost traffic.
        for address in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let probe = tokio::net::TcpStream::connect(SocketAddr::new(address, port));
            if let Ok(Ok(_)) =
                tokio::time::timeout(std::time::Duration::from_millis(300), probe).await
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another program is listening on {address}:{port}"),
                ));
            }
        }
        let v4 = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)).await?;
        let mut listeners = vec![v4];
        match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)).await {
            Ok(v6) => listeners.push(v6),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => return Err(error),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(error)
            }
            Err(_) => {}
        }
        Ok(Self { port, listeners })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        desktop_backend_base_url(self.port)
    }

    /// Serves until `stop` resolves or is dropped.
    pub async fn serve(
        self,
        relay: Arc<DesktopBackendRelay>,
        stop: tokio::sync::oneshot::Receiver<()>,
    ) {
        let app = guarded_router(relay, self.port);
        let servers = futures_util::future::join_all(self.listeners.into_iter().map(|listener| {
            let app = app.clone();
            async move { axum::serve(listener, app).await }
        }));
        tokio::select! {
            _ = servers => {}
            _ = stop => {}
        }
    }
}

/// A page on the web, as opposed to Desktop's own `app://` or `null`.
fn is_web_page_origin(origin: &axum::http::HeaderValue) -> bool {
    let origin = origin.to_str().unwrap_or("http:").to_ascii_lowercase();
    origin.starts_with("http:") || origin.starts_with("https:")
}

/// Host values a browser or Electron sends for `localhost` on `port`.
fn accepted_hosts(port: u16) -> Vec<String> {
    let mut hosts = vec![format!("localhost:{port}")];
    if port == 80 {
        hosts.push("localhost".into());
    }
    hosts
}

/// The relay surface behind its caller checks: a page in a browser carries a
/// web `Origin`, and a DNS-rebound name carries a foreign `Host`.
pub fn guarded_router(relay: Arc<DesktopBackendRelay>, port: u16) -> Router {
    let hosts = Arc::new(accepted_hosts(port));
    router(relay).layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let hosts = hosts.clone();
            async move {
                if request
                    .headers()
                    .get(axum::http::header::ORIGIN)
                    .is_some_and(is_web_page_origin)
                {
                    return StatusCode::FORBIDDEN.into_response();
                }
                let host = request
                    .headers()
                    .get(axum::http::header::HOST)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_ascii_lowercase);
                if !host.is_some_and(|host| hosts.contains(&host)) {
                    return StatusCode::MISDIRECTED_REQUEST.into_response();
                }
                next.run(request).await
            }
        },
    ))
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

    /// Desktop's own allowlist: exact host `localhost` or `localhost:8000`.
    #[test]
    fn base_urls_are_ones_desktop_will_send_its_login_to() {
        assert_eq!(
            desktop_backend_base_url(80),
            "http://localhost/desktop-backend/backend-api"
        );
        assert_eq!(
            desktop_backend_base_url(8000),
            "http://localhost:8000/desktop-backend/backend-api"
        );
        for port in DESKTOP_RELAY_PORTS {
            let url = url::Url::parse(&desktop_backend_base_url(port)).unwrap();
            let host = match url.port() {
                Some(port) => format!("{}:{port}", url.host_str().unwrap()),
                None => url.host_str().unwrap().to_string(),
            };
            assert!(host == "localhost" || host == "localhost:8000", "{host}");
        }
    }

    #[tokio::test]
    async fn an_occupied_port_is_not_taken_over() {
        let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = other.local_addr().unwrap().port();
        let error = DesktopRelayListener::bind_port(port).await.err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn browser_pages_and_foreign_hosts_are_refused() {
        use tower::ServiceExt;
        let relay = Arc::new(DesktopBackendRelay::with_upstreams(
            "http://127.0.0.1:9/backend-api",
            "http://127.0.0.1:9/appcast",
        ));
        let send = |port: u16, host: &str, origin: Option<&str>| {
            let mut builder = axum::http::Request::builder()
                .uri("/desktop-backend/backend-api/wham/usage")
                .header("host", host);
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            guarded_router(relay.clone(), port).oneshot(builder.body(Body::empty()).unwrap())
        };
        let status = |response: Response| response.status();
        assert_eq!(
            status(
                send(80, "localhost", Some("https://evil.example"))
                    .await
                    .unwrap()
            ),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(send(80, "localhost", Some("app://-")).await.unwrap()),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            status(send(80, "evil.example", None).await.unwrap()),
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            status(send(8000, "localhost", None).await.unwrap()),
            StatusCode::MISDIRECTED_REQUEST
        );
        // Admitted: reaches the (closed) upstream and reports it.
        assert_eq!(
            status(send(80, "localhost", None).await.unwrap()),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            status(send(8000, "localhost:8000", None).await.unwrap()),
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn backend_requests_keep_desktop_identity_and_usage_reads_unlimited() {
        use tower::ServiceExt;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, Option<String>)>::new()));
        let upstream = Router::new().route(
            "/backend-api/{*rest}",
            any({
                let seen = seen.clone();
                move |request: Request| {
                    let seen = seen.clone();
                    async move {
                        let auth = request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        seen.lock().unwrap().push((request.uri().to_string(), auth));
                        axum::Json(json!({
                            "rate_limit_reached_type": {"type": "workspace_owner_credits_depleted"},
                            "rate_limit": {"allowed": false, "limit_reached": true}
                        }))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });
        let relay = router(Arc::new(DesktopBackendRelay::with_upstreams(
            format!("http://{address}/backend-api"),
            format!("http://{address}/appcast"),
        )));
        for (path, unlimited) in [
            ("/desktop-backend/backend-api/wham/usage", true),
            (
                "/desktop-backend/backend-api/wham/tasks/list?limit=5",
                false,
            ),
        ] {
            let response = relay
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .header("authorization", "Bearer desktop-login")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["rate_limit"]["limit_reached"], !unlimited, "{path}");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/backend-api/wham/usage");
        assert_eq!(seen[1].0, "/backend-api/wham/tasks/list?limit=5");
        assert!(seen
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Bearer desktop-login")));
    }
}
