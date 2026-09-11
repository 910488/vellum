//! Thin upstream transport (plan §25.6).
//!
//! [`UpstreamTransport`] is deliberately dumb: method + URL + headers + body +
//! timeout in, status + headers + body out. It must never know anything about
//! Grok, Official, Chat, Responses, OAuth, continuation, compaction, or tool
//! restoration — all dialect knowledge lives above this interface. Keeping the
//! transport this narrow is what lets the same execution engine talk to a real
//! provider ([`ReqwestTransport`]) and to a scripted fake
//! ([`FixtureTransport`]) with identical semantics.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};

/// Why a transport-level call failed, before any provider semantics attach.
///
/// The runtime maps these into `RuntimeError` categories at one place
/// ([`crate::exec`]), so a connect refusal and a hang never both land as the
/// generic "internal error" a caller cannot act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The upstream did not answer within the request timeout.
    #[error("upstream request timed out: {0}")]
    Timeout(String),
    /// The upstream could not be reached (DNS / connection refused).
    #[error("failed to connect to upstream: {0}")]
    Connect(String),
    /// The request itself could not be built or sent (bad method/URL).
    #[error("invalid upstream request: {0}")]
    InvalidRequest(String),
    /// The upstream answered but its body could not be read.
    #[error("failed reading upstream response: {0}")]
    ResponseRead(String),
    /// The upstream body exceeded the caller's declared cap while being read.
    #[error("upstream response exceeded {limit} bytes (bytes_read={bytes_read})")]
    ResponseTooLarge { limit: usize, bytes_read: usize },
    /// Any other transport-level failure.
    #[error("upstream transport failure: {0}")]
    Other(String),
}

/// One upstream HTTP call. `method` and header names are strings so the type
/// stays transport-neutral and serializable; validation happens at the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Option<Duration>,
    /// Cap on the buffered upstream body. `None` means no overall size cap
    /// (Official native HTTP). Third-party non-stream responses set 64 MiB.
    pub max_response_bytes: Option<usize>,
}

/// The upstream's reply. `body` is raw bytes: whether it is JSON or SSE is
/// decided by the caller, not the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// An upstream SSE exchange. `body` is a byte stream that must be parsed as
/// SSE by the caller; the transport stays dialect-free.
pub struct UpstreamStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: BoxStream<'static, Result<axum::body::Bytes, TransportError>>,
}

#[async_trait]
pub trait UpstreamTransport: Send + Sync + 'static {
    async fn execute(&self, request: &UpstreamRequest) -> Result<UpstreamResponse, TransportError>;

    /// Streaming variant. The default implementation buffers through
    /// [`UpstreamTransport::execute`] and emits the whole body as one chunk,
    /// so transports that only implement buffered execution keep working
    /// unchanged; transports that can stream do so incrementally.
    async fn execute_streaming(
        &self,
        request: &UpstreamRequest,
    ) -> Result<UpstreamStream, TransportError> {
        let response = self.execute(request).await?;
        let body = futures_util::stream::once(async move {
            Ok::<axum::body::Bytes, TransportError>(axum::body::Bytes::from(response.body))
        })
        .boxed();
        Ok(UpstreamStream {
            status: response.status,
            headers: response.headers,
            body,
        })
    }
}

/// Bound TCP/TLS connect so a dead IPv6 path cannot stall first-byte by
/// minutes. Streaming still has no overall body deadline; only the handshake
/// is capped.
pub const PRODUCTION_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Production transport over `reqwest`.
///
/// The TLS client is built on first upstream call, not at proxy bind:
/// `/readyz` must not wait for rustls initialization.
pub struct ReqwestTransport {
    client: OnceLock<reqwest::Client>,
}

fn build_production_client() -> reqwest::Client {
    // Do not auto-follow redirects. A 3xx Location is a new destination
    // that must be re-checked (and must not inherit Authorization on a
    // different origin). Official URLs are not globally validated here;
    // disabling hops keeps that exemption from becoming a silent open
    // redirect. Callers observe 3xx as a normal upstream response.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PRODUCTION_CONNECT_TIMEOUT)
        .tcp_nodelay(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: OnceLock::new(),
        }
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        let holder = OnceLock::new();
        let _ = holder.set(client);
        Self { client: holder }
    }

    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(build_production_client)
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UpstreamTransport for ReqwestTransport {
    async fn execute(&self, request: &UpstreamRequest) -> Result<UpstreamResponse, TransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|error| {
            TransportError::InvalidRequest(format!(
                "invalid upstream method `{}`: {error}",
                request.method
            ))
        })?;
        let mut builder = self.client().request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let response = builder
            .body(request.body.clone())
            .send()
            .await
            .map_err(classify_reqwest_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let max_response_bytes = request.max_response_bytes;
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                TransportError::ResponseRead(format!(
                    "failed reading upstream response body: {error}"
                ))
            })?;
            let next = body.len().saturating_add(chunk.len());
            if let Some(limit) = max_response_bytes {
                if next > limit {
                    return Err(TransportError::ResponseTooLarge {
                        limit,
                        bytes_read: next,
                    });
                }
            }
            body.extend_from_slice(&chunk);
        }
        Ok(UpstreamResponse {
            status,
            headers,
            body,
        })
    }

    async fn execute_streaming(
        &self,
        request: &UpstreamRequest,
    ) -> Result<UpstreamStream, TransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|error| {
            TransportError::InvalidRequest(format!(
                "invalid upstream method `{}`: {error}",
                request.method
            ))
        })?;
        let mut builder = self.client().request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        // The overall timeout is intentionally not applied here: a stream may
        // legitimately outlive `request.timeout` between chunks. The caller
        // enforces per-chunk idle deadlines.
        let response = builder
            .body(request.body.clone())
            .send()
            .await
            .map_err(classify_reqwest_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let body = response
            .bytes_stream()
            .map(|item| item.map_err(classify_reqwest_error))
            .boxed();
        Ok(UpstreamStream {
            status,
            headers,
            body,
        })
    }
}

/// Map a `reqwest` failure to the typed transport error. `is_request` means
/// the request could not be built or sent at all (bad URL, redirect, ...) —
/// that is a defect in what the runtime asked for, not a provider failure.
fn classify_reqwest_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout(error.to_string())
    } else if error.is_connect() {
        TransportError::Connect(error.to_string())
    } else if error.is_request() {
        TransportError::InvalidRequest(error.to_string())
    } else {
        TransportError::Other(error.to_string())
    }
}

/// Deterministic fake transport for tests and parity runs. Serves scripted
/// responses in call order and never touches the network.
pub struct FixtureTransport {
    script: Vec<UpstreamResponse>,
    calls: std::sync::atomic::AtomicUsize,
}

impl FixtureTransport {
    pub fn new(script: Vec<UpstreamResponse>) -> Self {
        assert!(
            !script.is_empty(),
            "FixtureTransport needs at least one response"
        );
        Self {
            script,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl UpstreamTransport for FixtureTransport {
    async fn execute(
        &self,
        _request: &UpstreamRequest,
    ) -> Result<UpstreamResponse, TransportError> {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Exhausting the script repeats the last turn, mirroring FakeUpstream's
        // contract so probing requests stay deterministic.
        Ok(self.script[index.min(self.script.len() - 1)].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_transport_caps_connect_handshake() {
        assert_eq!(PRODUCTION_CONNECT_TIMEOUT, Duration::from_secs(10));
        let _transport = ReqwestTransport::new();
    }

    fn scripted() -> UpstreamResponse {
        UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"ok":true}"#.to_vec(),
        }
    }

    #[tokio::test]
    async fn fixture_transport_serves_script_in_order_and_repeats_the_last_turn() {
        let transport = FixtureTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{"turn":0}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{"turn":1}"#.to_vec(),
            },
        ]);
        let request = UpstreamRequest {
            method: "POST".into(),
            url: "http://nowhere.invalid/v1/responses".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
            max_response_bytes: None,
        };
        for index in 0..2 {
            let response = transport.execute(&request).await.unwrap();
            assert_eq!(response.body, format!(r#"{{"turn":{index}}}"#).into_bytes());
        }
        let repeated = transport.execute(&request).await.unwrap();
        assert_eq!(repeated.body, br#"{"turn":1}"#.to_vec());
        assert_eq!(transport.call_count(), 3);
    }

    #[tokio::test]
    async fn fixture_transport_is_used_as_a_boxed_trait_object() {
        let transport: Box<dyn UpstreamTransport> =
            Box::new(FixtureTransport::new(vec![scripted()]));
        let request = UpstreamRequest {
            method: "POST".into(),
            url: "http://nowhere.invalid/".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
            max_response_bytes: None,
        };
        let response = transport.execute(&request).await.unwrap();
        assert_eq!(response.status, 200);
    }

    /// Exercises the real `reqwest` path in-process so the transport contract
    /// (status + headers + body, plus timeout) is proven against a live HTTP
    /// exchange, not just a fixture.
    #[tokio::test]
    async fn reqwest_transport_round_trips_through_a_real_listener() {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::Arc;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct Seen(Arc<Mutex<Vec<String>>>);

        let seen = Seen(Arc::new(Mutex::new(Vec::new())));
        let router = Router::new()
            .route(
                "/v1/responses",
                post(|State(seen): State<Seen>, body: Bytes| async move {
                    seen.0
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body).to_string());
                    (StatusCode::OK, Json(serde_json::json!({"echo": true}))).into_response()
                }),
            )
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let transport = ReqwestTransport::new();
        let response = transport
            .execute(&UpstreamRequest {
                method: "POST".into(),
                url: format!("http://{address}/v1/responses"),
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"model":"m"}"#.to_vec(),
                timeout: Some(Duration::from_secs(5)),
                max_response_bytes: None,
            })
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body, serde_json::json!({"echo": true}));
        server.abort();
    }

    #[tokio::test]
    async fn reqwest_transport_does_not_auto_follow_redirects() {
        use axum::http::{header::LOCATION, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::Router;

        let router = Router::new()
            .route(
                "/from",
                get(|| async { (StatusCode::FOUND, [(LOCATION, "/to")]).into_response() }),
            )
            .route(
                "/to",
                get(|| async { (StatusCode::OK, "followed").into_response() }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let transport = ReqwestTransport::new();
        let response = transport
            .execute(&UpstreamRequest {
                method: "GET".into(),
                url: format!("http://{address}/from"),
                headers: Vec::new(),
                body: Vec::new(),
                timeout: Some(Duration::from_secs(5)),
                max_response_bytes: None,
            })
            .await
            .unwrap();
        assert_eq!(response.status, 302);
        assert!(
            !response
                .body
                .windows(b"followed".len())
                .any(|w| w == b"followed"),
            "the hop must not be followed automatically"
        );
        server.abort();
    }

    #[tokio::test]
    async fn reqwest_transport_surfaces_a_connection_failure_as_an_error() {
        let transport = ReqwestTransport::new();
        let result = transport
            .execute(&UpstreamRequest {
                method: "POST".into(),
                url: "http://127.0.0.1:1/v1/responses".into(),
                headers: Vec::new(),
                body: Vec::new(),
                timeout: Some(Duration::from_secs(2)),
                max_response_bytes: None,
            })
            .await;
        assert!(result.is_err());
    }
}
