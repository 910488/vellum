//! ACP's transport boundary: newline-delimited JSON-RPC on stdio.
//!
//! This crate intentionally does not interpret provider notifications. Adapters
//! map [`AcpIncoming`] into harness-neutral events.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot, Mutex};

const MAX_PROTOCOL_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("ACP transport closed")]
    Closed,
    #[error("ACP protocol error: {0}")]
    Protocol(String),
    #[error("ACP I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ACP JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ACP request was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcpIncoming {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    ProtocolError {
        detail: String,
    },
    Eof,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, AcpError>>>>>;
type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// A concurrent request client. JSON-RPC IDs are correlated independently of
/// receive order; notifications cannot block a response.
pub struct AcpClient {
    writer: SharedWriter,
    pending: Pending,
    incoming: broadcast::Sender<AcpIncoming>,
    next_id: Arc<Mutex<u64>>,
}

impl AcpClient {
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (incoming, _) = broadcast::channel(256);
        tokio::spawn(run_reader(reader, Arc::clone(&pending), incoming.clone()));
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending,
            incoming,
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AcpIncoming> {
        self.incoming.subscribe()
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next = next.saturating_add(1);
            id
        };
        let key = id.to_string();
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(key.clone(), sender);
        let frame = json!({"jsonrpc":"2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.write_frame(&frame).await {
            self.pending.lock().await.remove(&key);
            return Err(error);
        }
        receiver.await.map_err(|_| AcpError::Closed)?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        self.write_frame(&json!({"jsonrpc":"2.0", "method": method, "params": params}))
            .await
    }

    /// Replies to a server-originated JSON-RPC request, such as a permission
    /// request. The caller owns the native request ID and must resolve it once.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), AcpError> {
        self.write_frame(&json!({"jsonrpc":"2.0", "id": id, "result": result}))
            .await
    }

    async fn write_frame(&self, frame: &Value) -> Result<(), AcpError> {
        let encoded = serde_json::to_vec(frame)?;
        let mut writer = self.writer.lock().await;
        writer.write_all(&encoded).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

async fn run_reader<R>(reader: R, pending: Pending, incoming: broadcast::Sender<AcpIncoming>)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => {
                fail_all(&pending, AcpError::Closed).await;
                let _ = incoming.send(AcpIncoming::Eof);
                return;
            }
            Ok(size) if size > MAX_PROTOCOL_LINE_BYTES => {
                let detail = format!("protocol line exceeds {MAX_PROTOCOL_LINE_BYTES} byte limit");
                let _ = incoming.send(AcpIncoming::ProtocolError { detail });
                fail_all(
                    &pending,
                    AcpError::Protocol("oversized protocol line".into()),
                )
                .await;
                return;
            }
            Ok(_) => {}
            Err(error) => {
                fail_all(&pending, AcpError::Io(error)).await;
                return;
            }
        }
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                let _ = incoming.send(AcpIncoming::ProtocolError {
                    detail: format!("malformed JSON: {error}"),
                });
                continue;
            }
        };
        if value.get("id").is_some()
            && (value.get("result").is_some() || value.get("error").is_some())
            && value.get("method").is_none()
        {
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            let key = rpc_id_key(&id);
            let result = if let Some(result) = value.get("result") {
                Ok(result.clone())
            } else {
                Err(AcpError::Protocol(
                    value
                        .get("error")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "unknown JSON-RPC error".into()),
                ))
            };
            match pending.lock().await.remove(&key) {
                Some(sender) => {
                    let _ = sender.send(result);
                }
                None => {
                    let _ = incoming.send(AcpIncoming::ProtocolError {
                        detail: format!("unknown or duplicate response id {key}"),
                    });
                }
            }
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = value.get("id") {
                let _ = incoming.send(AcpIncoming::ServerRequest {
                    id: id.clone(),
                    method: method.into(),
                    params,
                });
            } else {
                let _ = incoming.send(AcpIncoming::Notification {
                    method: method.into(),
                    params,
                });
            }
        } else {
            let _ = incoming.send(AcpIncoming::ProtocolError {
                detail: "unrecognized JSON-RPC frame".into(),
            });
        }
    }
}

fn rpc_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".into())
}

async fn fail_all(pending: &Pending, error: AcpError) {
    let drained = std::mem::take(&mut *pending.lock().await);
    for (_, sender) in drained {
        let _ = sender.send(Err(AcpError::Protocol(error.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn correlates_out_of_order_responses_with_notification_interleaving() {
        let (client_read, mut agent_write) = duplex(4096);
        let (mut agent_read, client_write) = duplex(4096);
        let client = Arc::new(AcpClient::new(client_read, client_write));
        let mut events = client.subscribe();
        let a_client = Arc::clone(&client);
        let a = tokio::spawn(async move { a_client.request("a", Value::Null).await });
        let b_client = Arc::clone(&client);
        let b = tokio::spawn(async move { b_client.request("b", Value::Null).await });
        let mut lines = BufReader::new(&mut agent_read).lines();
        assert!(lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .contains("\"id\":1"));
        assert!(lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .contains("\"id\":2"));
        agent_write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":\"two\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"one\"}\n").await.unwrap();
        assert_eq!(b.await.unwrap().unwrap(), Value::String("two".into()));
        assert!(matches!(
            events.recv().await.unwrap(),
            AcpIncoming::Notification { .. }
        ));
        assert_eq!(a.await.unwrap().unwrap(), Value::String("one".into()));
    }

    #[tokio::test]
    async fn eof_fails_pending_requests_and_notifies_subscribers() {
        let (client_read, agent_write) = duplex(4096);
        let (mut agent_read, client_write) = duplex(4096);
        let client = Arc::new(AcpClient::new(client_read, client_write));
        let mut events = client.subscribe();
        let request_client = Arc::clone(&client);
        let pending =
            tokio::spawn(async move { request_client.request("session/prompt", json!({})).await });
        let mut lines = BufReader::new(&mut agent_read).lines();
        assert!(lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .contains("session/prompt"));
        drop(agent_write);
        let error = pending.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("closed"));
        assert!(matches!(events.recv().await.unwrap(), AcpIncoming::Eof));
    }

    #[tokio::test]
    async fn malformed_and_unknown_response_ids_are_reported_without_breaking_reader() {
        let (client_read, mut agent_write) = duplex(4096);
        let (_agent_read, client_write) = duplex(4096);
        let client = AcpClient::new(client_read, client_write);
        let mut events = client.subscribe();
        agent_write
            .write_all(b"not json\n{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}\n")
            .await
            .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(), AcpIncoming::ProtocolError { detail } if detail.contains("malformed JSON"))
        );
        assert!(
            matches!(events.recv().await.unwrap(), AcpIncoming::ProtocolError { detail } if detail.contains("unknown or duplicate"))
        );
    }
}
