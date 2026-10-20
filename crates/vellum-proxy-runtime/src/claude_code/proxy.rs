use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::usage::ResponseInspector;
use super::{account_hash, TokenUsage, UsageObservation};

const HOST: &str = "api.anthropic.com";
const MAX_BODY: usize = 32 * 1024 * 1024;

/// Secret-bearing lease: deliberately not Debug or Serialize.
pub struct AccountToken {
    pub account_id: String,
    pub access_token: String,
}

#[async_trait]
pub trait ClaudeAccounts: Send + Sync {
    /// Snapshot the selection for this request. None explicitly means A→A probe.
    async fn selected_token(&self) -> Result<Option<AccountToken>, String>;
    /// Attribute a response to the account captured at request admission.
    async fn observe_usage(&self, _account_id: &str, _usage: UsageObservation) {}
    /// One finished Desktop inference request. Called from a drop path, so it
    /// must not wait on async work.
    fn record_request(&self, _record: &CodeUsageRecord) {}
}

/// What one Desktop inference request did, for usage rows and logs. Carries no
/// credential or content: only the account id and already hashed identities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeUsageRecord {
    pub request_id: String,
    pub session_hash: String,
    /// Raw execution account id; None means the original authorization passed.
    pub account_id: Option<String>,
    pub operation: String,
    pub model: Option<String>,
    pub upstream_status: Option<u16>,
    /// completed, upstream_rejected, interrupted or transport_failed.
    pub outcome: String,
    pub error_type: Option<String>,
    pub retry_after: Option<u64>,
    pub usage: TokenUsage,
    pub duration_ms: u64,
    pub first_byte_ms: Option<u64>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRouteStatus {
    pub connect_requests: u64,
    pub tls_failures: u64,
    pub api_requests: u64,
    pub last_error: Option<String>,
    pub active_requests: usize,
    pub desktop_requests: u64,
    pub records: Vec<CodeRequestRecord>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRequestRecord {
    pub id: String,
    pub session_hash: String,
    pub account_id: Option<String>,
    pub operation: String,
    pub state: String,
    pub upstream_status: Option<u16>,
}

/// User settings also affect CLI tool children. Authenticated opaque tunnels
/// in that scope must support their private services and package downloads.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum ProxyScope {
    #[default]
    Project,
    User,
}

/// Does not own settings or Desktop. Stopping it never touches a Code process.
pub struct ClaudeCodeProxy {
    authorization: String,
    scope: ProxyScope,
    accounts: Arc<dyn ClaudeAccounts>,
    client: reqwest::Client,
    upstream: String,
    status: Mutex<CodeRouteStatus>,
    requests: Arc<tokio::sync::Semaphore>,
    accepting: AtomicBool,
    tunnels: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    tunnel_slots: Arc<tokio::sync::Semaphore>,
}

impl ClaudeCodeProxy {
    pub fn new(secret: &str, accounts: Arc<dyn ClaudeAccounts>) -> Result<Arc<Self>, String> {
        Self::with_scope(secret, accounts, ProxyScope::Project)
    }

    pub fn with_scope(
        secret: &str,
        accounts: Arc<dyn ClaudeAccounts>,
        scope: ProxyScope,
    ) -> Result<Arc<Self>, String> {
        Self::build_scoped(secret, accounts, "https://api.anthropic.com".into(), scope)
    }

    fn build_scoped(
        secret: &str,
        accounts: Arc<dyn ClaudeAccounts>,
        upstream: String,
        scope: ProxyScope,
    ) -> Result<Arc<Self>, String> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(180))
            .build()
            .map_err(|_| "Cannot create Claude upstream transport")?;
        Ok(Arc::new(Self {
            scope,
            authorization: format!("Basic {}", STANDARD.encode(format!("vellum:{secret}"))),
            accounts,
            client,
            upstream,
            status: Mutex::new(CodeRouteStatus::default()),
            requests: Arc::new(tokio::sync::Semaphore::new(32)),
            accepting: AtomicBool::new(true),
            tunnels: Mutex::new(Vec::new()),
            tunnel_slots: Arc::new(tokio::sync::Semaphore::new(64)),
        }))
    }

    pub fn status(&self) -> CodeRouteStatus {
        let mut status = self.status.lock().unwrap().clone();
        // Include metadata/profile work and token refresh, not only inference
        // records, so the UI can offer an explicit stop whenever drain is busy.
        status.active_requests = 32 - self.requests.available_permits();
        status
    }

    /// Atomically stop admission before checking all admitted HTTP work, including
    /// credential refresh and non-inference requests that have no route record.
    pub fn begin_drain(&self) -> bool {
        self.accepting.store(false, Ordering::SeqCst);
        self.requests.available_permits() == 32
    }
    pub fn cancel_drain(&self) {
        self.accepting.store(true, Ordering::SeqCst);
    }

    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                accepted = listener.accept() => {
                    let Ok((socket, peer)) = accepted else { break; };
                    if !peer.ip().is_loopback() { continue; }
                    let proxy = self.clone();
                    let config = tls.clone();
                    let cancel = stop.clone();
                    tasks.spawn(async move {
                        let connection = hyper::server::conn::http1::Builder::new()
                            .header_read_timeout(Duration::from_secs(15))
                            .timer(hyper_util::rt::TokioTimer::new())
                            .serve_connection(TokioIo::new(socket), service_fn(move |request| {
                                let proxy = proxy.clone(); let tls = config.clone(); let cancel = cancel.clone();
                                async move { Ok::<_, Infallible>(proxy.connect(request, tls, cancel).await) }
                            })).with_upgrades();
                        let _ = connection.await;
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        let tunnels = std::mem::take(&mut *self.tunnels.lock().unwrap());
        for task in &tunnels {
            task.abort();
        }
        for task in tunnels {
            let _ = task.await;
        }
    }

    async fn connect(
        self: Arc<Self>,
        mut request: Request<hyper::body::Incoming>,
        tls: Arc<rustls::ServerConfig>,
        mut cancel: watch::Receiver<bool>,
    ) -> Response {
        if request
            .headers()
            .get(header::PROXY_AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some(&self.authorization)
        {
            let mut response = failure(
                StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                "proxy_authentication_required",
            );
            response.headers_mut().insert(
                header::PROXY_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Vellum Claude Code\""),
            );
            return response;
        }
        if request.method() == Method::GET && request.uri().path() == "/readyz" {
            return Response::new(Body::from("ready"));
        }
        if request.method() != Method::CONNECT {
            return failure(StatusCode::METHOD_NOT_ALLOWED, "connect_required");
        }
        let Some(authority) = request.uri().authority() else {
            return failure(StatusCode::BAD_REQUEST, "invalid_connect_target");
        };
        // URI authority keeps IPv6 brackets; lookup_host expects the bare IP.
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let port = authority.port_u16().unwrap_or(443);
        let opaque_user = self.scope == ProxyScope::User && host != HOST;
        if host.is_empty() || port == 0 || (!opaque_user && authority.port_u16() != Some(443)) {
            return failure(StatusCode::FORBIDDEN, "https_port_required");
        }
        // Keep MITM and project admission bounded. Opaque user tunnels do not
        // consume this budget: inherited proxy env must not cap cargo/npm work.
        let tunnel_slot = if opaque_user {
            None
        } else {
            match self.tunnel_slots.clone().try_acquire_owned() {
                Ok(slot) => Some(slot),
                Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "claude_tunnel_limit"),
            }
        };
        self.status.lock().unwrap().connect_requests += 1;
        // Resolve once and connect to those exact addresses. Only the user
        // scope permits private destinations, after proxy-secret validation.
        let raw = if host != HOST {
            let addresses = match tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            {
                Ok(Ok(addresses)) => addresses.collect::<Vec<_>>(),
                _ => return failure(StatusCode::BAD_GATEWAY, "tunnel_dns_failed"),
            };
            if addresses.is_empty()
                || (!opaque_user && addresses.iter().any(|a| !public_address(a.ip())))
            {
                return failure(StatusCode::FORBIDDEN, "private_tunnel_refused");
            }
            // Offline fixtures can exercise real loopback tunnels, but must
            // never reach an external/private service on the developer's LAN.
            #[cfg(test)]
            if self.upstream.starts_with("http://127.0.0.1:")
                && addresses.iter().any(|a| !a.ip().is_loopback())
            {
                return failure(StatusCode::FORBIDDEN, "offline_external_tunnel_refused");
            }
            match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addresses[..]))
                .await
            {
                Ok(Ok(socket)) => Some(socket),
                _ => return failure(StatusCode::BAD_GATEWAY, "tunnel_connect_failed"),
            }
        } else {
            None
        };
        let upgrade = hyper::upgrade::on(&mut request);
        // The task watches the same generation. Existing tunnels close on Stop too.
        let owner = self.clone();
        let task = tokio::spawn(async move {
            let _tunnel_slot = tunnel_slot;
            let work = async move {
                let Ok(upgraded) = upgrade.await else {
                    return;
                };
                let mut socket = TokioIo::new(upgraded);
                if let Some(mut raw) = raw {
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut raw).await;
                    return;
                }
                let handshake = tokio::time::timeout(
                    Duration::from_secs(15),
                    tokio_rustls::TlsAcceptor::from(tls).accept(socket),
                )
                .await;
                let stream = match handshake {
                    Ok(Ok(stream)) => stream,
                    error => {
                        let code = match error {
                            Ok(Err(error)) if format!("{error:?}").contains("UnknownCA") => {
                                "client_ca_untrusted"
                            }
                            Err(_) => "client_tls_timeout",
                            _ => "client_tls_failed",
                        };
                        let mut status = self.status.lock().unwrap();
                        status.tls_failures += 1;
                        status.last_error = Some(code.into());
                        return;
                    }
                };
                if stream.get_ref().1.server_name() != Some(HOST) {
                    return;
                }
                let connection = hyper::server::conn::http1::Builder::new()
                    .header_read_timeout(Duration::from_secs(15))
                    .timer(hyper_util::rt::TokioTimer::new())
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request| {
                            let proxy = self.clone();
                            async move {
                                Ok::<_, Infallible>(proxy.forward(request.map(Body::new)).await)
                            }
                        }),
                    );
                let _ = connection.await;
            };
            tokio::select! { _ = work => {}, _ = cancel.changed() => {} }
        });
        let mut tunnels = owner.tunnels.lock().unwrap();
        tunnels.retain(|task| !task.is_finished());
        tunnels.push(task);
        Response::new(Body::empty())
    }

    async fn forward(self: Arc<Self>, request: Request<Body>) -> Response {
        let Ok(permit) = self.requests.clone().try_acquire_owned() else {
            return failure(StatusCode::SERVICE_UNAVAILABLE, "claude_proxy_busy");
        };
        if !self.accepting.load(Ordering::SeqCst) {
            return failure(StatusCode::SERVICE_UNAVAILABLE, "claude_proxy_stopping");
        }
        self.status.lock().unwrap().api_requests += 1;
        let (parts, body) = request.into_parts();
        if parts.uri.scheme().is_some()
            || parts.uri.authority().is_some()
            || !matches!(
                parts
                    .headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok()),
                Some("api.anthropic.com" | "api.anthropic.com:443")
            )
        {
            return failure(StatusCode::BAD_REQUEST, "anthropic_host_required");
        }
        let bytes =
            match tokio::time::timeout(Duration::from_secs(60), to_bytes(body, MAX_BODY)).await {
                Ok(Ok(bytes)) => bytes,
                _ => {
                    return failure(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_body_unavailable_or_too_large",
                    )
                }
            };
        let inference = parts.method == Method::POST
            && matches!(
                parts.uri.path(),
                "/v1/messages" | "/v1/messages/count_tokens"
            );
        let desktop = is_desktop_code(&parts.headers);
        let mut headers = end_to_end_headers(&parts.headers);
        let mut guard = None;
        let mut response_account = None;
        if inference && desktop {
            self.status.lock().unwrap().last_error = None;
            let (session, model) = match session_key(&parts.headers, &bytes) {
                Ok(identity) => identity,
                Err(code) => {
                    log::warn!("[ClaudeCode] request refused: {code}");
                    return failure(StatusCode::CONFLICT, code);
                }
            };
            let lease = match self.accounts.selected_token().await {
                Ok(lease) => lease,
                Err(error) => {
                    log::warn!("[ClaudeCode] execution account unavailable: {error}");
                    return failure(
                        StatusCode::UNAUTHORIZED,
                        "claude_execution_account_unavailable",
                    );
                }
            };
            let account_id = lease.as_ref().map(|l| l.account_id.clone());
            response_account = account_id.clone();
            if let Some(lease) = lease {
                let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", lease.access_token))
                else {
                    return failure(StatusCode::UNAUTHORIZED, "invalid_execution_credential");
                };
                headers.insert(header::AUTHORIZATION, value);
                headers.remove("x-api-key");
            }
            let operation = if parts.uri.path() == "/v1/messages" {
                "messages"
            } else {
                "count_tokens"
            };
            guard = Some(RequestGuard::new(
                self.clone(),
                session,
                account_id,
                operation,
                model,
            ));
        }
        let path = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let result = self
            .client
            .request(parts.method, format!("{}{path}", self.upstream))
            .headers(headers)
            .body(bytes)
            .send()
            .await;
        let upstream = match result {
            Ok(response) => response,
            Err(error) => {
                log::warn!(
                    "[ClaudeCode] upstream transport failed timeout={} connect={}",
                    error.is_timeout(),
                    error.is_connect()
                );
                if let Some(guard) = guard.as_mut() {
                    guard.finish("transport_failed");
                }
                return failure(StatusCode::BAD_GATEWAY, "claude_upstream_transport_failed");
            }
        };
        if let Some(account_id) = response_account {
            let usage = UsageObservation::from_headers(upstream.headers());
            if !usage.is_empty() {
                let accounts = self.accounts.clone();
                // Account refresh/usage API reads may hold their lock for
                // seconds. Never delay response headers or streaming for it.
                tokio::spawn(async move {
                    accounts.observe_usage(&account_id, usage).await;
                });
            }
        }
        let status = upstream.status();
        if let Some(guard) = guard.as_mut() {
            guard.update("streaming", Some(status.as_u16()));
            guard.response(upstream.headers(), status.as_u16());
        }
        let expected_length = upstream.content_length();
        if expected_length == Some(0) {
            if let Some(guard) = guard.as_mut() {
                guard.finish(if status.is_success() {
                    "completed"
                } else {
                    "upstream_rejected"
                });
            }
        }
        let headers = end_to_end_headers(upstream.headers());
        let stream = async_stream::stream! {
            let _permit = permit;
            let mut guard = guard;
            let mut received=0u64;
            let mut bytes = upstream.bytes_stream();
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(chunk) => {
                        received=received.saturating_add(chunk.len() as u64);
                        if let Some(guard)=guard.as_mut(){guard.inspect(&chunk);}
                        // Hyper can drop a fixed-length body after its last bytes
                        // without polling EOF; commit upstream completion first.
                        if expected_length==Some(received) {if let Some(guard)=guard.as_mut(){guard.finish(if status.is_success(){"completed"}else{"upstream_rejected"});}}
                        yield Ok::<Bytes,std::io::Error>(chunk);
                    }
                    Err(_) => { log::warn!("[ClaudeCode] upstream stream interrupted"); yield Err(std::io::Error::other("Claude upstream stream interrupted")); return; }
                }
            }
            if let Some(guard) = guard.as_mut() { guard.finish(if status.is_success() {"completed"} else {"upstream_rejected"}); }
        };
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }
}

struct RequestGuard {
    proxy: Arc<ClaudeCodeProxy>,
    id: String,
    done: bool,
    started: std::time::Instant,
    inspector: Option<ResponseInspector>,
    record: CodeUsageRecord,
}
impl RequestGuard {
    fn new(
        proxy: Arc<ClaudeCodeProxy>,
        session_hash: String,
        account_id: Option<String>,
        operation: &str,
        model: Option<String>,
    ) -> Self {
        let id = ulid::Ulid::new().to_string();
        let record = CodeUsageRecord {
            request_id: id.clone(),
            session_hash: session_hash.clone(),
            account_id: account_id.clone(),
            operation: operation.into(),
            model,
            ..Default::default()
        };
        {
            let mut status = proxy.status.lock().unwrap();
            status.active_requests += 1;
            status.desktop_requests += 1;
            // Never evict live requests, and bound admission separately below.
            if status.records.len() >= 64 {
                if let Some(index) = status
                    .records
                    .iter()
                    .position(|r| !matches!(r.state.as_str(), "sending" | "streaming"))
                {
                    status.records.remove(index);
                }
            }
            status.records.push(CodeRequestRecord {
                id: id.clone(),
                session_hash,
                account_id,
                operation: operation.into(),
                state: "sending".into(),
                upstream_status: None,
            });
        }
        Self {
            proxy,
            id,
            done: false,
            started: std::time::Instant::now(),
            inspector: None,
            record,
        }
    }
    fn response(&mut self, headers: &HeaderMap, status: u16) {
        self.record.upstream_status = Some(status);
        self.record.first_byte_ms = Some(elapsed_ms(self.started));
        self.record.retry_after = headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        self.inspector = Some(ResponseInspector::new(headers));
    }
    fn inspect(&mut self, chunk: &[u8]) {
        if let Some(inspector) = self.inspector.as_mut() {
            inspector.feed(chunk);
        }
    }
    fn update(&self, state: &str, code: Option<u16>) {
        if let Some(record) = self
            .proxy
            .status
            .lock()
            .unwrap()
            .records
            .iter_mut()
            .find(|r| r.id == self.id)
        {
            record.state = state.into();
            if code.is_some() {
                record.upstream_status = code;
            }
        }
    }
    fn finish(&mut self, state: &str) {
        self.update(state, None);
        self.done = true;
        self.record.outcome = state.into();
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.done {
            self.update("interrupted", None);
            self.record.outcome = "interrupted".into();
        }
        {
            let mut status = self.proxy.status.lock().unwrap();
            status.active_requests = status.active_requests.saturating_sub(1);
        }
        if let Some(mut inspector) = self.inspector.take() {
            inspector.finish();
            self.record.usage = inspector.usage;
            self.record.error_type = inspector.error_type;
        }
        self.record.duration_ms = elapsed_ms(self.started);
        log_record(&self.record);
        self.proxy.accounts.record_request(&self.record);
    }
}

fn elapsed_ms(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// One line per Desktop inference request. The account column, request after
/// request, is what shows a switch.
fn log_record(record: &CodeUsageRecord) {
    let account = record
        .account_id
        .as_deref()
        .map(account_hash)
        .unwrap_or_else(|| "passthrough".into());
    let status = record
        .upstream_status
        .map_or_else(|| "-".into(), |s| s.to_string());
    let usage = record.usage;
    let line = format!(
        "[ClaudeCode] request={} op={} session={} account={account} model={} status={status} outcome={} input={} cache_read={} cache_write={} output={} duration_ms={}",
        record.request_id,
        record.operation,
        record.session_hash,
        record.model.as_deref().unwrap_or("-"),
        record.outcome,
        usage.input_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
        usage.output_tokens,
        record.duration_ms,
    );
    if record.outcome == "completed" {
        log::info!("{line}");
    } else {
        log::warn!(
            "{line} error={} retry_after={}",
            record.error_type.as_deref().unwrap_or("-"),
            record
                .retry_after
                .map_or_else(|| "-".into(), |s| s.to_string())
        );
    }
}

fn is_desktop_code(headers: &HeaderMap) -> bool {
    if headers.get_all(header::USER_AGENT).iter().count() != 1 {
        return false;
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ua.starts_with("claude-cli/")
        && ua
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .any(|part| part == "claude-desktop")
}

/// Hashed session identity and the requested model.
fn session_key(
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<(String, Option<String>), &'static str> {
    let mut decoded = Vec::new();
    let bytes = match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => bytes,
        Some("gzip") => {
            use std::io::Read;
            flate2::read::GzDecoder::new(bytes)
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|_| "invalid_gzip_body")?;
            if decoded.len() > MAX_BODY {
                return Err("decoded_request_too_large");
            }
            &decoded
        }
        _ => return Err("unsupported_request_encoding"),
    };
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid_messages_json")?;
    let raw = value
        .pointer("/metadata/user_id")
        .and_then(|v| v.as_str())
        .ok_or("claude_session_identity_missing")?;
    let parsed = serde_json::from_str::<serde_json::Value>(raw).ok();
    let id = parsed
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(|v| v.as_str())
        .or_else(|| raw.rsplit_once("_session_").map(|(_, id)| id))
        .ok_or("claude_session_identity_unsupported")?;
    if id.len() < 8
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("claude_session_identity_invalid");
    }
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| {
            !m.is_empty()
                && m.len() <= 128
                && m.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:@[]".contains(&c))
        })
        .map(str::to_owned);
    Ok((
        hex::encode(Sha256::digest(id.as_bytes()))[..16].into(),
        model,
    ))
}

fn end_to_end_headers(source: &HeaderMap) -> HeaderMap {
    let mut result = source.clone();
    if let Some(value) = source.get(header::CONNECTION).and_then(|v| v.to_str().ok()) {
        for key in value.split(',') {
            result.remove(key.trim());
        }
    }
    for key in [
        "connection",
        "proxy-authorization",
        "proxy-authenticate",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "host",
    ] {
        result.remove(key);
    }
    result
}

fn public_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.octets()[0] == 0)
        }
        std::net::IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|v| public_address(v.into()))
            .unwrap_or_else(|| {
                !(ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local())
            }),
    }
}

fn failure(status: StatusCode, code: &str) -> Response {
    let mut response=Response::new(Body::from(serde_json::json!({"type":"error","error":{"type":"vellum_claude_code_error","message":code}}).to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests;
