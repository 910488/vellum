//! Unix-socket WebSocket transport for official Codex app-server.
//!
//! Production path:
//! `UnixStream -> HTTP Upgrade -> WebSocket text frames -> JSON-RPC`
//!
//! Connection lifecycle:
//! split -> writer pump -> reader pump -> initialize -> Ready
//! disconnect -> Recovering -> backoff reconnect -> epoch++

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};

use super::jsonrpc::{
    classify_incoming_str, ClientNotification, ClientRequest, ClientResponse, IncomingMessage,
    JsonRpcId,
};
use super::runtime::ResolvedCodexRuntime;
use super::transport::{
    AppServerError, AppServerTransport, ServerIdentity, UpstreamConnectionState, UpstreamMessage,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_BASE_MS: u64 = 250;
const RECONNECT_MAX_MS: u64 = 5_000;

type PendingMap = HashMap<JsonRpcId, oneshot::Sender<Result<Value, AppServerError>>>;

#[derive(Debug, Clone)]
pub struct UnixWsEndpoint {
    pub socket_path: PathBuf,
}

impl UnixWsEndpoint {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn display(&self) -> String {
        format!("unix://{}", self.socket_path.display())
    }
}

#[allow(dead_code)]
enum OutboundMessage {
    Text(String),
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct UnixWsAppServerTransport {
    socket_path: PathBuf,
    expected_version: String,
    codex_home: Option<String>,
    epoch: Arc<AtomicU64>,
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    events: broadcast::Sender<UpstreamMessage>,
    outbound_tx: Arc<RwLock<Option<mpsc::UnboundedSender<OutboundMessage>>>>,
    connection_state: Arc<RwLock<UpstreamConnectionState>>,
    reader_alive: Arc<AtomicBool>,
    writer_alive: Arc<AtomicBool>,
    identity: Arc<Mutex<Option<ServerIdentity>>>,
    stop: Arc<AtomicBool>,
}

impl UnixWsAppServerTransport {
    pub async fn connect(socket_path: impl Into<PathBuf>) -> Result<Arc<Self>, AppServerError> {
        Self::connect_with_runtime(socket_path, "unknown".to_string(), None).await
    }

    pub async fn connect_with_runtime(
        socket_path: impl Into<PathBuf>,
        expected_version: impl Into<String>,
        codex_home: Option<String>,
    ) -> Result<Arc<Self>, AppServerError> {
        let socket_path = socket_path.into();
        let expected_version = expected_version.into();
        let (events, _) = broadcast::channel(512);
        let transport = Arc::new(Self {
            socket_path: socket_path.clone(),
            expected_version,
            codex_home,
            epoch: Arc::new(AtomicU64::new(0)),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events,
            outbound_tx: Arc::new(RwLock::new(None)),
            connection_state: Arc::new(RwLock::new(UpstreamConnectionState::Connecting)),
            reader_alive: Arc::new(AtomicBool::new(false)),
            writer_alive: Arc::new(AtomicBool::new(false)),
            identity: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        });
        transport.clone().spawn_supervisor();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if transport.connection_state() == UpstreamConnectionState::Ready {
                break;
            }
            if transport.connection_state() == UpstreamConnectionState::Incompatible {
                return Err(AppServerError::Incompatible(
                    "upstream became incompatible during connect".into(),
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(transport)
    }

    pub async fn connect_resolved(
        runtime: &ResolvedCodexRuntime,
    ) -> Result<Arc<Self>, AppServerError> {
        Self::connect_with_runtime(
            runtime.socket_path.clone(),
            runtime.binary_version.clone(),
            Some(runtime.codex_home.display().to_string()),
        )
        .await
    }

    fn spawn_supervisor(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut backoff_ms = RECONNECT_BASE_MS;
            while !self.stop.load(Ordering::SeqCst) {
                match self.run_one_connection().await {
                    Ok(()) => {
                        backoff_ms = RECONNECT_BASE_MS;
                    }
                    Err(error) => {
                        log::warn!("upstream connection attempt failed: {error}");
                        self.set_state(UpstreamConnectionState::Recovering).await;
                    }
                }
                if self.stop.load(Ordering::SeqCst) {
                    break;
                }
                self.set_state(UpstreamConnectionState::Recovering).await;
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms.saturating_mul(2)).min(RECONNECT_MAX_MS);
            }
            self.set_state(UpstreamConnectionState::Stopped).await;
        });
    }

    async fn run_one_connection(self: &Arc<Self>) -> Result<(), AppServerError> {
        #[cfg(unix)]
        {
            self.run_unix_connection().await
        }
        #[cfg(not(unix))]
        {
            self.set_state(UpstreamConnectionState::Incompatible).await;
            Err(AppServerError::Transport(
                "Unix socket WebSocket transport is only available on Unix hosts".into(),
            ))
        }
    }

    #[cfg(unix)]
    async fn run_unix_connection(self: &Arc<Self>) -> Result<(), AppServerError> {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::UnixStream;
        use tokio_tungstenite::client_async;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message;

        self.set_state(UpstreamConnectionState::Connecting).await;
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| {
                AppServerError::Transport(format!(
                    "failed to connect unix socket {}: {error}",
                    self.socket_path.display()
                ))
            })?;
        let request = "ws://localhost/"
            .into_client_request()
            .map_err(|error| AppServerError::Transport(error.to_string()))?;
        let (ws, _) = client_async(request, stream).await.map_err(|error| {
            AppServerError::Transport(format!("websocket upgrade failed: {error}"))
        })?;
        let (mut sink, mut stream) = ws.split();

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut slot = self.outbound_tx.write().await;
            *slot = Some(outbound_tx);
        }

        // Capture epoch for this connection before any frame handling.
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.reader_alive.store(true, Ordering::SeqCst);
        self.writer_alive.store(true, Ordering::SeqCst);

        let writer_flag = self.writer_alive.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                match message {
                    OutboundMessage::Text(text) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            writer_flag.store(false, Ordering::SeqCst);
        });

        // Reader must run BEFORE initialize so the response can complete.
        let reader_self = Arc::clone(self);
        let reader_flag = self.reader_alive.clone();
        let reader_task = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => {
                        if let Err(error) = reader_self.handle_incoming_text(epoch, &text).await {
                            log::warn!("upstream frame handling failed: {error}");
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            reader_flag.store(false, Ordering::SeqCst);
        });

        // Give reader task a chance to start before first RPC.
        tokio::task::yield_now().await;

        if let Err(error) = self.perform_initialize(epoch).await {
            self.reader_alive.store(false, Ordering::SeqCst);
            self.writer_alive.store(false, Ordering::SeqCst);
            {
                let mut slot = self.outbound_tx.write().await;
                *slot = None;
            }
            writer_task.abort();
            reader_task.abort();
            self.fail_all_pending("initialize failed").await;
            let _ = self.events.send(UpstreamMessage::Disconnected {
                epoch,
                reason: format!("initialize failed: {error}"),
            });
            return Err(error);
        }

        // Wait until reader dies (socket closed / error).
        let _ = reader_task.await;
        writer_task.abort();
        self.reader_alive.store(false, Ordering::SeqCst);
        self.writer_alive.store(false, Ordering::SeqCst);
        {
            let mut slot = self.outbound_tx.write().await;
            *slot = None;
        }
        self.fail_all_pending("upstream disconnected").await;
        let _ = self.events.send(UpstreamMessage::Disconnected {
            epoch,
            reason: format!("upstream websocket closed (epoch={epoch})"),
        });
        self.set_state(UpstreamConnectionState::Recovering).await;
        Ok(())
    }

    #[allow(dead_code)]
    async fn perform_initialize(&self, epoch: u64) -> Result<(), AppServerError> {
        self.set_state(UpstreamConnectionState::Initializing).await;
        let result = self
            .call_internal(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "vellum_remote_broker",
                        "title": "Vellum Remote Broker",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await?;

        let identity = ServerIdentity {
            name: result
                .get("userAgent")
                .and_then(Value::as_str)
                .unwrap_or("codex-app-server")
                .to_string(),
            version: self.expected_version.clone(),
            platform: result
                .get("platformOs")
                .and_then(Value::as_str)
                .map(str::to_string),
            platform_family: result
                .get("platformFamily")
                .and_then(Value::as_str)
                .map(str::to_string),
            platform_os: result
                .get("platformOs")
                .and_then(Value::as_str)
                .map(str::to_string),
            codex_home: result
                .get("codexHome")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.codex_home.clone()),
        };
        *self.identity.lock().await = Some(identity.clone());
        self.notify("initialized", Value::Null).await?;
        self.set_state(UpstreamConnectionState::Ready).await;
        let _ = self.events.send(UpstreamMessage::Ready { epoch, identity });
        log::info!(
            "upstream ready epoch={epoch} version={}",
            self.expected_version
        );
        Ok(())
    }

    async fn set_state(&self, state: UpstreamConnectionState) {
        *self.connection_state.write().await = state;
    }

    #[allow(dead_code)]
    async fn handle_incoming_text(&self, epoch: u64, text: &str) -> Result<(), AppServerError> {
        let message = classify_incoming_str(text).map_err(AppServerError::Protocol)?;
        match message {
            IncomingMessage::Response(response) => {
                let mut pending = self.pending.lock().await;
                if let Some(tx) = pending.remove(&response.id) {
                    if let Some(error) = response.error {
                        let _ = tx.send(Err(AppServerError::Protocol(error.to_string())));
                    } else {
                        let _ = tx.send(Ok(response.result.unwrap_or(Value::Null)));
                    }
                }
            }
            IncomingMessage::Request(request) => {
                let _ = self.events.send(UpstreamMessage::ServerRequest {
                    epoch,
                    id: request.id,
                    method: request.method,
                    params: request.params,
                });
            }
            IncomingMessage::Notification(note) => {
                let _ = self.events.send(UpstreamMessage::Notification {
                    epoch,
                    method: note.method,
                    params: note.params,
                });
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn fail_all_pending(&self, reason: &str) {
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(AppServerError::Transport(reason.into())));
        }
    }

    async fn send_text(&self, value: &impl Serialize) -> Result<(), AppServerError> {
        let text = serde_json::to_string(value)
            .map_err(|error| AppServerError::Protocol(error.to_string()))?;
        let guard = self.outbound_tx.read().await;
        let Some(tx) = guard.as_ref() else {
            return Err(AppServerError::NotReady);
        };
        tx.send(OutboundMessage::Text(text))
            .map_err(|_| AppServerError::Transport("upstream writer closed".into()))
    }

    async fn call_internal(&self, method: &str, params: Value) -> Result<Value, AppServerError> {
        if !self.writer_alive.load(Ordering::SeqCst) {
            return Err(AppServerError::NotReady);
        }
        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let id = JsonRpcId::from(id_num);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), tx);
        }
        let request = ClientRequest::new(id.clone(), method, params);
        if let Err(error) = self.send_text(&request).await {
            let mut pending = self.pending.lock().await;
            pending.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AppServerError::Transport("pending request dropped".into())),
            Err(_) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                Err(AppServerError::Timeout)
            }
        }
    }
}

#[async_trait]
impl AppServerTransport for UnixWsAppServerTransport {
    async fn initialize(&self) -> Result<ServerIdentity, AppServerError> {
        if let Some(identity) = self.identity.lock().await.clone() {
            if self.connection_state() == UpstreamConnectionState::Ready {
                return Ok(identity);
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            if self.connection_state() == UpstreamConnectionState::Ready {
                if let Some(identity) = self.identity.lock().await.clone() {
                    return Ok(identity);
                }
            }
            if self.connection_state() == UpstreamConnectionState::Incompatible {
                return Err(AppServerError::Incompatible("upstream incompatible".into()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(AppServerError::NotReady)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, AppServerError> {
        if self.connection_state() != UpstreamConnectionState::Ready {
            return Err(AppServerError::NotReady);
        }
        self.call_internal(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), AppServerError> {
        self.send_text(&ClientNotification::new(method, params))
            .await
    }

    async fn respond(&self, id: JsonRpcId, result: Value) -> Result<(), AppServerError> {
        self.send_text(&ClientResponse::result(id, result)).await
    }

    fn subscribe(&self) -> broadcast::Receiver<UpstreamMessage> {
        self.events.subscribe()
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn connection_state(&self) -> UpstreamConnectionState {
        self.connection_state
            .try_read()
            .map(|g| *g)
            .unwrap_or(UpstreamConnectionState::Recovering)
    }

    fn reader_alive(&self) -> bool {
        self.reader_alive.load(Ordering::SeqCst)
    }

    fn writer_alive(&self) -> bool {
        self.writer_alive.load(Ordering::SeqCst)
    }
}

pub fn socket_exists(path: &Path) -> bool {
    path.exists()
}
