//! JSON-RPC transport for the facade.
//!
//! The facade answers method calls; this module is what a client actually
//! talks to. It is transport-agnostic on purpose — it runs over any
//! `AsyncRead`/`AsyncWrite` pair, so the same server serves stdio, a pipe, or a
//! socket without changing a line of protocol handling.
//!
//! The connection is bidirectional. Besides answering client requests it also
//! *originates* requests (permission approvals) and correlates the client's
//! responses back to the facade. A client that never answers cannot wedge the
//! server: outbound traffic is queued independently of the read loop.
//!
//! Framing is newline-delimited JSON, matching the ACP side of Vellum. stdout
//! carries protocol only; diagnostics belong on stderr.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::facade::{CodexAppServerFacade, CodexFacadeError};
use crate::ui_mapper::CodexServerMessage;

/// Refuse a frame larger than this rather than buffering without bound.
const MAX_PROTOCOL_LINE_BYTES: usize = 8 * 1024 * 1024;

/// JSON-RPC error codes. The first three are the spec's; the last is the
/// implementation-defined range, used for harness and permission failures so a
/// client can tell "you asked wrongly" from "the harness refused".
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HARNESS_ERROR: i64 = -32000;

fn error_body(error: &CodexFacadeError) -> Value {
    let (code, category) = match error {
        CodexFacadeError::UnsupportedMethod(_) => (METHOD_NOT_FOUND, "unsupportedMethod"),
        CodexFacadeError::InvalidRequest(_) => (INVALID_PARAMS, "invalidRequest"),
        CodexFacadeError::Permission(_) => (HARNESS_ERROR, "permission"),
        CodexFacadeError::Harness(_) => (HARNESS_ERROR, "harness"),
    };
    json!({
        "code": code,
        "message": error.to_string(),
        "data": {"category": category}
    })
}

/// Serves one client connection until the reader reaches EOF.
///
/// Returns when the client disconnects. A malformed frame is reported to the
/// client and the connection continues; only EOF or an I/O failure ends it.
pub async fn serve<R, W>(
    facade: Arc<CodexAppServerFacade>,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    // Outbound pump: notifications and server-originated requests. It runs
    // independently so a slow or silent client cannot block request handling.
    let mut outbound = facade.subscribe();
    let outbound_writer = Arc::clone(&writer);
    let pump = tokio::spawn(async move {
        while let Ok(message) = outbound.recv().await {
            let frame = match message {
                CodexServerMessage::Notification(notification) => json!({
                    "jsonrpc": "2.0",
                    "method": notification.method,
                    "params": notification.params
                }),
                // The permission id doubles as the JSON-RPC id: JSON-RPC
                // permits string ids, and reusing it keeps one correlation
                // key across the whole approval round trip.
                CodexServerMessage::Request { id, method, params } => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params
                }),
            };
            if write_frame(&outbound_writer, &frame).await.is_err() {
                return;
            }
        }
    });

    let mut lines = BufReader::new(reader);
    let mut line = Vec::new();
    let result = loop {
        line.clear();
        match lines.read_until(b'\n', &mut line).await {
            Ok(0) => break Ok(()),
            Ok(size) if size > MAX_PROTOCOL_LINE_BYTES => {
                break Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "protocol line exceeds the size limit",
                ));
            }
            Ok(_) => {}
            Err(error) => break Err(error),
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let frame: Value = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(error) => {
                let _ = write_frame(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": format!("parse error: {error}")}
                    }),
                )
                .await;
                continue;
            }
        };
        if let Some(frame) = handle_frame(&facade, frame).await {
            if write_frame(&writer, &frame).await.is_err() {
                break Ok(());
            }
        }
    };

    pump.abort();
    result
}

/// Returns the frame to write back, if the inbound frame calls for one.
async fn handle_frame(facade: &CodexAppServerFacade, frame: Value) -> Option<Value> {
    let id = frame.get("id").cloned();

    // A frame carrying a result or error and no method is the client answering
    // one of our server requests, e.g. a permission decision.
    if frame.get("method").is_none() {
        let Some(Value::String(request_id)) = id else {
            return None;
        };
        if let Some(result) = frame.get("result") {
            if let Err(error) = facade.handle_response(&request_id, result.clone()).await {
                eprintln!("vellum-facade: permission response rejected: {error}");
            }
        } else {
            // An error reply is a refusal, not an absent answer: it still has
            // to reach the native harness so the tool call is not left hanging.
            if let Err(error) = facade.handle_response(&request_id, json!({})).await {
                eprintln!("vellum-facade: permission refusal rejected: {error}");
            }
        }
        return None;
    }

    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(json!({}));
    let response = facade.handle_request(method, params).await;

    // A notification (no id) gets no reply, however it turned out.
    let id = id?;
    Some(match response {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error_body(&error)}),
    })
}

async fn write_frame<W>(writer: &Arc<tokio::sync::Mutex<W>>, frame: &Value) -> std::io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    let mut encoded = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    let mut writer = writer.lock().await;
    writer.write_all(&encoded).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::HarnessBroker;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::broadcast;
    use vellum_harness_protocol::{
        CompactionAuthority, HarnessCapabilities, HarnessEvent, HarnessEventEnvelope, HarnessId,
        HarnessSelection, HarnessSessionBinding, HarnessSessionHandle, PermissionRequestEvent,
    };
    use vellum_harness_runtime::{
        CancelRequest, CreateSessionSpec, HarnessError, HarnessSession, NativeSessionSummary,
        PermissionResolution, PromptHandle, PromptRequest,
    };

    struct StubSession {
        handle: HarnessSessionHandle,
        events: broadcast::Sender<HarnessEventEnvelope>,
        resolutions: Mutex<Vec<PermissionResolution>>,
    }

    #[async_trait]
    impl HarnessSession for StubSession {
        fn handle(&self) -> &HarnessSessionHandle {
            &self.handle
        }
        fn capabilities(&self) -> &HarnessCapabilities {
            static CAPS: std::sync::OnceLock<HarnessCapabilities> = std::sync::OnceLock::new();
            CAPS.get_or_init(|| HarnessCapabilities {
                permissions: true,
                compaction: true,
                ..Default::default()
            })
        }
        async fn prompt(&self, _: PromptRequest) -> Result<PromptHandle, HarnessError> {
            Ok(PromptHandle {
                native_turn_id: Some("native-turn".into()),
            })
        }
        async fn cancel(&self, _: CancelRequest) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn resolve_permission(
            &self,
            resolution: PermissionResolution,
        ) -> Result<(), HarnessError> {
            self.resolutions.lock().unwrap().push(resolution);
            Ok(())
        }
        async fn set_model(&self, _: String) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn set_reasoning_effort(&self, _: String) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn compact(&self) -> Result<(), HarnessError> {
            Ok(())
        }
        fn subscribe(&self) -> broadcast::Receiver<HarnessEventEnvelope> {
            self.events.subscribe()
        }
    }

    struct StubBroker {
        session: Arc<StubSession>,
    }

    #[async_trait]
    impl HarnessBroker for StubBroker {
        async fn create_session(
            &self,
            _: HarnessSelection,
            _: CreateSessionSpec,
        ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
            Ok(self.session.clone())
        }
        async fn resume_session(&self, _: &str) -> Result<Arc<dyn HarnessSession>, HarnessError> {
            Ok(self.session.clone())
        }
        async fn session_for_thread(
            &self,
            _: &str,
        ) -> Result<Arc<dyn HarnessSession>, HarnessError> {
            Ok(self.session.clone())
        }
        async fn list_native_sessions(
            &self,
            _: &str,
        ) -> Result<Vec<NativeSessionSummary>, HarnessError> {
            Ok(Vec::new())
        }
        async fn binding_for_thread(&self, _: &str) -> Result<HarnessSessionBinding, HarnessError> {
            Err(HarnessError::unsupported("bindingForThread"))
        }
        async fn compaction_authority(&self, _: &str) -> Result<CompactionAuthority, HarnessError> {
            Ok(CompactionAuthority::NativeHarness)
        }
    }

    fn harness() -> (Arc<CodexAppServerFacade>, Arc<StubSession>) {
        let (events, _) = broadcast::channel(64);
        let session = Arc::new(StubSession {
            handle: HarnessSessionHandle {
                runtime_instance_id: "runtime".into(),
                native_session_id: "native".into(),
            },
            events,
            resolutions: Mutex::new(Vec::new()),
        });
        let facade = Arc::new(CodexAppServerFacade::new(Arc::new(StubBroker {
            session: session.clone(),
        })));
        (facade, session)
    }

    /// A minimal JSON-RPC client speaking the same framing as a real one.
    struct TestClient {
        writer: tokio::io::DuplexStream,
        reader: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    }

    impl TestClient {
        async fn send(&mut self, frame: Value) {
            self.writer
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn next(&mut self) -> Value {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(5), self.reader.next_line())
                    .await
                    .expect("server answered within the deadline")
                    .unwrap()
                    .expect("server did not close the connection");
            serde_json::from_str(&line).unwrap()
        }
        /// Reads until a frame satisfying `predicate` arrives.
        async fn next_matching(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
            loop {
                let frame = self.next().await;
                if predicate(&frame) {
                    return frame;
                }
            }
        }
    }

    fn connect(facade: Arc<CodexAppServerFacade>) -> TestClient {
        // Two independent pipes, one per direction, so the client's writes and
        // the server's writes never share a buffer.
        let (client_writer, server_reader) = duplex(64 * 1024);
        let (server_writer, client_reader) = duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = serve(facade, server_reader, server_writer).await;
        });
        TestClient {
            writer: client_writer,
            reader: BufReader::new(client_reader).lines(),
        }
    }

    /// Opens a thread over the wire and returns its id.
    async fn start_thread(client: &mut TestClient, workspace: &std::path::Path) -> String {
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "thread/start",
                "params": {
                    "cwd": workspace.to_string_lossy(),
                    "harnessSelection": {"harnessId": "grok-build"}
                }
            }))
            .await;
        let response = client
            .next_matching(|frame| frame.get("id") == Some(&json!(1)))
            .await;
        response["result"]["thread"]["id"]
            .as_str()
            .expect("thread/start returns an id")
            .to_owned()
    }

    fn permission_event(thread_id: &str) -> HarnessEventEnvelope {
        HarnessEventEnvelope {
            seq: 1,
            harness_id: HarnessId("grok-build".into()),
            thread_id: thread_id.to_owned(),
            native_session_id: "native".into(),
            native_event_id: None,
            occurred_at: chrono::Utc::now(),
            event: HarnessEvent::PermissionRequested(PermissionRequestEvent {
                native_request_id: "call-1".into(),
                description: "write file".into(),
                payload: json!({"cwd": "C:/workspace"}),
            }),
        }
    }

    async fn wait_for_resolution(session: &StubSession) -> PermissionResolution {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(resolution) = session.resolutions.lock().unwrap().first().cloned() {
                return resolution;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the permission decision never reached the harness"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn an_unknown_method_is_answered_with_method_not_found() {
        let (facade, _) = harness();
        let mut client = connect(facade);
        client
            .send(json!({"jsonrpc": "2.0", "id": 1, "method": "thread/archive", "params": {}}))
            .await;
        let response = client
            .next_matching(|frame| frame.get("id") == Some(&json!(1)))
            .await;
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
        assert_eq!(
            response["error"]["data"]["category"],
            json!("unsupportedMethod")
        );
    }

    #[tokio::test]
    async fn a_permission_round_trips_over_the_wire() {
        let workspace = tempfile::tempdir().unwrap();
        let (facade, session) = harness();
        let mut client = connect(Arc::clone(&facade));
        let thread_id = start_thread(&mut client, workspace.path()).await;

        // The harness raises a permission request.
        session.events.send(permission_event(&thread_id)).unwrap();

        let request = client
            .next_matching(|frame| {
                frame.get("method") == Some(&json!("item/permissions/requestApproval"))
            })
            .await;
        let request_id = request["id"]
            .as_str()
            .expect("a server request carries an id");
        // The UI sees a Vellum id, never the native one.
        assert!(request_id.starts_with("perm_"));
        assert_eq!(request["params"]["reason"], json!("write file"));

        // The UI answers with a JSON-RPC response, as Codex clients do.
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"permissions": {"fileSystem": "write"}, "scope": "turn"}
            }))
            .await;

        let resolution = wait_for_resolution(&session).await;
        assert!(resolution.granted);
        assert_eq!(resolution.native_request_id, "call-1");
    }

    #[tokio::test]
    async fn an_error_reply_to_a_permission_is_a_refusal_not_a_dropped_request() {
        let workspace = tempfile::tempdir().unwrap();
        let (facade, session) = harness();
        let mut client = connect(Arc::clone(&facade));
        let thread_id = start_thread(&mut client, workspace.path()).await;
        session.events.send(permission_event(&thread_id)).unwrap();

        let request = client
            .next_matching(|frame| {
                frame.get("method") == Some(&json!("item/permissions/requestApproval"))
            })
            .await;
        let request_id = request["id"].as_str().unwrap().to_owned();

        // A client that rejects the request must still unblock the harness.
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "user declined"}
            }))
            .await;

        let resolution = wait_for_resolution(&session).await;
        assert!(
            !resolution.granted,
            "an error reply must reach the harness as a refusal"
        );
    }

    #[tokio::test]
    async fn notifications_reach_the_client_without_being_asked_for() {
        let workspace = tempfile::tempdir().unwrap();
        let (facade, _) = harness();
        let mut client = connect(facade);
        let thread_id = start_thread(&mut client, workspace.path()).await;
        // `thread/started` is emitted, not requested.
        let notification = client
            .next_matching(|frame| frame.get("method") == Some(&json!("thread/started")))
            .await;
        assert_eq!(notification["params"]["thread"]["id"], json!(thread_id));
        assert!(notification.get("id").is_none(), "a notification has no id");
    }

    #[tokio::test]
    async fn turn_start_returns_the_native_turn_id_used_by_harness_events() {
        let workspace = tempfile::tempdir().unwrap();
        let (facade, _) = harness();
        let mut client = connect(facade);
        let thread_id = start_thread(&mut client, workspace.path()).await;

        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "ping"}]
                }
            }))
            .await;
        let response = client
            .next_matching(|frame| frame.get("id") == Some(&json!(2)))
            .await;

        assert_eq!(response["result"]["turn"]["id"], json!("native-turn"));
    }

    #[tokio::test]
    async fn a_malformed_frame_is_reported_without_closing_the_connection() {
        let (facade, _) = harness();
        let mut client = connect(facade);
        client.writer.write_all(b"not json\n").await.unwrap();
        let parse_error = client
            .next_matching(|frame| frame.get("error").is_some())
            .await;
        assert_eq!(parse_error["error"]["code"], json!(-32700));

        // The connection still serves the next request.
        client
            .send(json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}}))
            .await;
        let response = client
            .next_matching(|frame| frame.get("id") == Some(&json!(7)))
            .await;
        assert!(response["result"]["userAgent"]["name"]
            .as_str()
            .is_some_and(|name| name.contains("vellum")));
    }

    #[tokio::test]
    async fn a_client_notification_is_acted_on_but_not_answered() {
        let (facade, _) = harness();
        let mut client = connect(facade);
        // No id: a JSON-RPC notification. It must not draw a response frame.
        client
            .send(json!({"jsonrpc": "2.0", "method": "initialize", "params": {}}))
            .await;
        client
            .send(json!({"jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {}}))
            .await;
        let first = client.next().await;
        assert_eq!(
            first["id"],
            json!(9),
            "the notification must not have produced a frame of its own"
        );
    }
}
