//! Read-only native session observability (M34 P0 fix).
//!
//! Queries the Codex app-server control API (the session authority) at the
//! daemon control endpoint under `$CODEX_HOME`. The Unix socket accepts a
//! standard WebSocket HTTP upgrade before JSON-RPC text frames and requires
//! the `initialize` → `initialized` handshake before `thread/*` methods. It
//! never reads Broker or Desktop state. When the
//! daemon is not running or the app-server does not answer, this returns an
//! explicit `NativeSessionObservabilityUnsupported` error instead of
//! fabricating data.

#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Turn status values from the current Codex app-server schema.
pub const TURN_STATUS_IN_PROGRESS: &str = "inProgress";
pub const TURN_STATUS_COMPLETED: &str = "completed";
pub const TURN_STATUS_INTERRUPTED: &str = "interrupted";
pub const TURN_STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeSessionStatus {
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub daemon_version: Option<String>,
    /// `nativeAppServer` when the control API answered, `unsupported` when the
    /// capability is unavailable on this Codex version / host.
    pub observability: String,
    pub threads: Vec<NativeThreadStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeThreadStatus {
    pub thread_id: String,
    pub status: String,
    pub active: bool,
    pub active_turn_id: Option<String>,
    pub turn_count: u64,
    pub last_turn_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approvals: Option<bool>,
}

/// Query the native app-server control API. `thread_id` narrows to a single
/// thread (thread/read with turns); `None` lists all threads.
pub fn query_session_status(
    codex_home: &Path,
    thread_id: Option<&str>,
) -> Result<NativeSessionStatus, String> {
    let native = crate::native_codex::discover_native()?;
    if !native.daemon_running {
        return Err(
            "NativeSessionObservabilityUnsupported: native app-server daemon is not running".into(),
        );
    }
    let mut status = NativeSessionStatus {
        daemon_running: true,
        daemon_pid: native.daemon_pid,
        daemon_version: native.daemon_version,
        observability: "unsupported".into(),
        threads: Vec::new(),
    };

    let Some(thread_id) = thread_id else {
        let payload = ws_jsonrpc_call(codex_home, "thread/list", serde_json::json!({}))
            .map_err(unsupported)?;
        status.threads = parse_thread_list(&payload).map_err(unsupported)?;
        // `thread/list` never populates `turns`; read each active candidate so
        // observers get a real active turn id, not an empty guess.
        let active_ids = status
            .threads
            .iter()
            .filter(|thread| thread.active)
            .map(|thread| thread.thread_id.clone())
            .collect::<Vec<_>>();
        for active_id in active_ids {
            let read = ws_jsonrpc_call(
                codex_home,
                "thread/read",
                serde_json::json!({ "threadId": active_id, "includeTurns": true }),
            )
            .map_err(unsupported)?;
            let detail = parse_thread_read(&read, &active_id).map_err(unsupported)?;
            if let Some(entry) = status
                .threads
                .iter_mut()
                .find(|entry| entry.thread_id == active_id)
            {
                *entry = detail;
            }
        }
        status.observability = "nativeAppServer".into();
        return Ok(status);
    };

    let payload = ws_jsonrpc_call(
        codex_home,
        "thread/read",
        serde_json::json!({ "threadId": thread_id, "includeTurns": true }),
    )
    .map_err(unsupported)?;
    let thread = parse_thread_read(&payload, thread_id).map_err(unsupported)?;
    // Keep the full list when the daemon answers it; the target thread is the
    // first entry so observers can rely on position.
    if let Ok(list) = ws_jsonrpc_call(codex_home, "thread/list", serde_json::json!({})) {
        let mut all = parse_thread_list(&list).map_err(unsupported)?;
        if let Some(entry) = all.iter_mut().find(|entry| entry.thread_id == thread_id) {
            *entry = thread.clone();
        } else {
            all.insert(0, thread.clone());
        }
        status.threads = all;
    } else {
        status.threads = vec![thread];
    }
    status.observability = "nativeAppServer".into();
    Ok(status)
}

fn unsupported(detail: String) -> String {
    format!("NativeSessionObservabilityUnsupported: {detail}")
}

/// Strict mapping of a `thread/list` response (`data: Vec<Thread>`).
/// JSON-RPC errors, missing data and malformed entries all fail closed.
fn parse_thread_list(payload: &Value) -> Result<Vec<NativeThreadStatus>, String> {
    if let Some(error) = payload.get("error") {
        return Err(format!("app-server error response: {error}"));
    }
    let data = payload
        .pointer("/result/data")
        .and_then(Value::as_array)
        .ok_or_else(|| "thread/list response missing result.data".to_string())?;
    data.iter().map(map_thread).collect::<Result<Vec<_>, _>>()
}

/// Strict mapping of a `thread/read` response (`result.thread`). Never
/// fabricates an entry: JSON-RPC errors, missing thread data, an identity
/// mismatch with the requested id, and malformed status/turns all fail.
fn parse_thread_read(payload: &Value, expected: &str) -> Result<NativeThreadStatus, String> {
    if let Some(error) = payload.get("error") {
        return Err(format!("app-server error response: {error}"));
    }
    let thread = payload
        .pointer("/result/thread")
        .ok_or_else(|| "thread/read response missing result.thread".to_string())?;
    let status = map_thread(thread)?;
    if status.thread_id != expected {
        return Err(format!(
            "thread/read identity mismatch: requested {expected}, returned {}",
            status.thread_id
        ));
    }
    Ok(status)
}

fn map_thread(thread: &Value) -> Result<NativeThreadStatus, String> {
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "thread entry missing id".to_string())?
        .to_string();
    let status = thread
        .get("status")
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
        .ok_or_else(|| format!("thread {thread_id} missing status"))?
        .to_string();
    let active = status == "active";
    let turns = thread.get("turns").and_then(Value::as_array);
    let (active_turn_id, last_turn_status, turn_count) = if let Some(turns) = turns {
        let last = turns.last();
        (
            last.and_then(|turn| turn.get("id").and_then(Value::as_str))
                .map(str::to_string),
            last.and_then(|turn| turn.get("status").and_then(Value::as_str))
                .map(str::to_string),
            turns.len() as u64,
        )
    } else {
        (
            thread
                .get("activeTurnId")
                .and_then(Value::as_str)
                .map(str::to_string),
            None,
            0,
        )
    };
    let (active_tools, pending_approvals) = if let Some(turns) = thread.get("turns").and_then(Value::as_array)
    {
        let tools = turns.iter().any(|turn| {
            turn.get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind.contains("tool") || kind.contains("command"))
                        && item.get("status").and_then(Value::as_str) == Some("inProgress")
                })
        });
        let approvals = turns.iter().any(|turn| {
            turn.get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind.contains("approval"))
                        && item.get("status").and_then(Value::as_str) != Some("completed")
                })
        });
        (Some(tools), Some(approvals))
    } else if !active {
        (Some(false), Some(false))
    } else {
        (None, None)
    };
    Ok(NativeThreadStatus {
        thread_id,
        status,
        active,
        active_turn_id,
        turn_count,
        last_turn_status,
        active_tools,
        pending_approvals,
    })
}

/// One JSON-RPC call over the daemon control socket. The caller reports any
/// failure as `NativeSessionObservabilityUnsupported`.
pub(crate) fn ws_jsonrpc_call(
    codex_home: &Path,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<Value, String>>();
    let codex_home = codex_home.to_path_buf();
    let method = method.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(ws_jsonrpc_call_inner(&codex_home, &method, params));
    });
    rx.recv_timeout(Duration::from_secs(20))
        .map_err(|_| "native control API timed out".to_string())?
}

#[cfg(unix)]
fn ws_jsonrpc_call_inner(codex_home: &Path, method: &str, params: Value) -> Result<Value, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create session observer runtime: {error}"))?;
    runtime.block_on(ws_jsonrpc_call_async(codex_home, method, params))
}

#[cfg(not(unix))]
fn ws_jsonrpc_call_inner(
    _codex_home: &Path,
    _method: &str,
    _params: Value,
) -> Result<Value, String> {
    Err("unix sockets are unavailable on this platform".into())
}

#[cfg(unix)]
async fn ws_jsonrpc_call_async(
    codex_home: &Path,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    use futures_util::SinkExt;
    use tokio::net::UnixStream;
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let socket_path = codex_home
        .join("app-server-control")
        .join("app-server-control.sock");
    let stream = UnixStream::connect(&socket_path).await.map_err(|error| {
        format!(
            "connect native control socket {}: {error}",
            socket_path.display()
        )
    })?;
    let request = "ws://localhost/"
        .into_client_request()
        .map_err(|error| format!("build websocket request: {error}"))?;
    let (mut websocket, _) = client_async(request, stream)
        .await
        .map_err(|error| format!("native control websocket upgrade failed: {error}"))?;

    websocket
        .send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "vellum_remote_agent",
                        "title": "Vellum Remote Agent",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| format!("send initialize: {error}"))?;
    let initialize = read_jsonrpc_response(&mut websocket, 1).await?;
    if let Some(error) = initialize.get("error") {
        return Err(format!("initialize failed: {error}"));
    }

    websocket
        .send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": null
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| format!("send initialized: {error}"))?;
    websocket
        .send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": method,
                "params": params
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|error| format!("send {method}: {error}"))?;
    read_jsonrpc_response(&mut websocket, 2).await
}

#[cfg(unix)]
async fn read_jsonrpc_response<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    expected_id: u64,
) -> Result<Value, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    loop {
        match websocket.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(value);
                    }
                }
            }
            Some(Ok(Message::Ping(payload))) => websocket
                .send(Message::Pong(payload))
                .await
                .map_err(|error| format!("send websocket pong: {error}"))?,
            Some(Ok(Message::Close(_))) | None => {
                return Err("native control websocket closed".into());
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(format!("read native control websocket: {error}")),
        }
    }
}

// Retained only as low-level frame fixtures for compatibility tests. The
// production observer uses tokio-tungstenite above so HTTP upgrade, masking,
// ping/pong, fragmentation and close semantics stay standards-compliant.
#[allow(dead_code)]
struct ServerFrame {
    opcode: u8,
    payload: Vec<u8>,
}

#[cfg(unix)]
fn send_notification(stdin: &mut std::process::ChildStdin, method: &str) -> Result<(), String> {
    let payload = serde_json::json!({ "method": method });
    write_frame(
        stdin,
        &serde_json::to_vec(&payload).map_err(|error| error.to_string())?,
    )
}

#[cfg(unix)]
fn send_request(
    stdin: &mut std::process::ChildStdin,
    id: u64,
    method: &str,
    params: &Value,
) -> Result<(), String> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    write_frame(
        stdin,
        &serde_json::to_vec(&payload).map_err(|error| error.to_string())?,
    )
}

#[cfg(unix)]
fn write_frame(stdin: &mut std::process::ChildStdin, payload: &[u8]) -> Result<(), String> {
    use std::io::Write;
    stdin
        .write_all(&encode_client_frame(payload))
        .map_err(|error| format!("write frame: {error}"))
}

#[cfg(unix)]
fn read_response(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut std::process::ChildStdout,
    expected_id: u64,
) -> Result<Value, String> {
    loop {
        let frame = read_server_frame_from(stdout)?;
        match frame.opcode {
            0x1 => {
                // One JSON-RPC message per text frame; notifications with a
                // different id (or none) are ignored.
                let text = String::from_utf8_lossy(&frame.payload);
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(value);
                    }
                }
            }
            0x8 => return Err("app-server closed the WebSocket".into()),
            0x9 => {
                // Client-to-server frames must be masked (RFC 6455).
                use std::io::Write;
                stdin
                    .write_all(&encode_masked_frame(0x8a, &frame.payload))
                    .map_err(|error| format!("pong write failed: {error}"))?;
            }
            0x0 | 0x2 => {}
            other => return Err(format!("unexpected frame opcode {other:#x}")),
        }
    }
}

#[cfg(unix)]
fn read_server_frame_from(stream: &mut std::process::ChildStdout) -> Result<ServerFrame, String> {
    let mut header = [0u8; 2];
    read_exact(stream, &mut header).map_err(|error| format!("read frame header: {error}"))?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7f) as u64;
    if length == 126 {
        let mut extended = [0u8; 2];
        read_exact(stream, &mut extended).map_err(|error| format!("read ext length: {error}"))?;
        length = u16::from_be_bytes(extended) as u64;
    } else if length == 127 {
        let mut extended = [0u8; 8];
        read_exact(stream, &mut extended).map_err(|error| format!("read ext length: {error}"))?;
        length = u64::from_be_bytes(extended);
    }
    let mut mask = [0u8; 4];
    if masked {
        read_exact(stream, &mut mask).map_err(|error| format!("read mask: {error}"))?;
    }
    let mut payload = vec![0u8; length as usize];
    read_exact(stream, &mut payload).map_err(|error| format!("read payload: {error}"))?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(ServerFrame { opcode, payload })
}

#[cfg(unix)]
fn read_exact(stream: &mut std::process::ChildStdout, buffer: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut offset = 0;
    while offset < buffer.len() {
        let read = stream.read(&mut buffer[offset..])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        offset += read;
    }
    Ok(())
}

#[cfg(unix)]
fn encode_client_frame(payload: &[u8]) -> Vec<u8> {
    encode_masked_frame(0x81, payload)
}

#[cfg(unix)]
fn encode_masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let length = payload.len();
    if length < 126 {
        frame.push(0x80 | length as u8);
    } else if length <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    let mask: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_list_payload_maps_threads_and_active_state() {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "data": [
                    {
                        "id": "t-1",
                        "status": {"type": "active", "activeFlags": []},
                        "turns": [
                            {"id": "turn-1", "status": "completed"},
                            {"id": "turn-2", "status": "inProgress"}
                        ]
                    },
                    {
                        "id": "t-2",
                        "status": {"type": "idle"}
                    }
                ],
                "nextCursor": null
            }
        });
        let threads = parse_thread_list(&payload).unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id, "t-1");
        assert!(threads[0].active);
        assert_eq!(threads[0].active_turn_id.as_deref(), Some("turn-2"));
        assert_eq!(threads[0].turn_count, 2);
        assert_eq!(threads[0].last_turn_status.as_deref(), Some("inProgress"));
        assert_eq!(threads[1].status, "idle");
        assert!(!threads[1].active);
        assert_eq!(threads[1].turn_count, 0);
    }

    #[test]
    fn thread_read_payload_maps_target_thread() {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "thread": {
                    "id": "t-9",
                    "status": {"type": "active", "activeFlags": ["waitingOnApproval"]},
                    "turns": [{"id": "turn-5", "status": "inProgress"}]
                }
            }
        });
        let thread = parse_thread_read(&payload, "t-9").unwrap();
        assert_eq!(thread.thread_id, "t-9");
        assert!(thread.active);
        assert_eq!(thread.active_turn_id.as_deref(), Some("turn-5"));
        assert_eq!(thread.last_turn_status.as_deref(), Some("inProgress"));
    }

    #[test]
    fn missing_or_malformed_payloads_fail_closed_without_fabrication() {
        assert!(
            parse_thread_list(&serde_json::json!({"result": {"data": []}}))
                .unwrap()
                .is_empty()
        );
        // JSON-RPC error response fails.
        assert!(parse_thread_list(&serde_json::json!({"error": {"code": -32601}})).is_err());
        assert!(
            parse_thread_read(&serde_json::json!({"error": {"code": -32601}}), "t-missing")
                .is_err()
        );
        // Missing result.thread fails instead of fabricating the requested id.
        assert!(parse_thread_read(&serde_json::json!({"result": {}}), "t-missing").is_err());
        // Identity mismatch fails.
        let wrong = serde_json::json!({
            "result": {"thread": {"id": "t-other", "status": {"type": "idle"}}}
        });
        assert!(parse_thread_read(&wrong, "t-missing").is_err());
        // Missing status fails.
        let no_status = serde_json::json!({"result": {"thread": {"id": "t-1"}}});
        assert!(parse_thread_read(&no_status, "t-1").is_err());
    }

    #[test]
    fn unsupported_error_prefix_is_stable() {
        assert_eq!(
            unsupported("socket missing".into()),
            "NativeSessionObservabilityUnsupported: socket missing"
        );
    }
}
