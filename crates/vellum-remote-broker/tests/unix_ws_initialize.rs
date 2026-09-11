//! Integration coverage for Unix socket WebSocket initialize ordering.
//! Compiles on all targets; only runs the live socket path on Unix.

#[cfg(unix)]
mod unix_live {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;
    use vellum_remote_broker::app_server::transport::{
        AppServerTransport, UpstreamConnectionState,
    };
    use vellum_remote_broker::app_server::unix_ws::UnixWsAppServerTransport;

    #[tokio::test]
    async fn unix_ws_initialize_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app-server.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (initialized_tx, initialized_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut stream) = ws.split();

            // Expect initialize request first.
            let frame = stream.next().await.unwrap().unwrap();
            let text = frame.to_text().unwrap().to_string();
            let req: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                req.get("method").and_then(Value::as_str),
                Some("initialize")
            );
            let id = req.get("id").cloned().unwrap();

            let response = json!({
                "id": id,
                "result": {
                    "userAgent": "codex_app_server",
                    "codexHome": "/tmp/codex",
                    "platformFamily": "unix",
                    "platformOs": "linux"
                }
            });
            sink.send(tokio_tungstenite::tungstenite::Message::Text(
                response.to_string().into(),
            ))
            .await
            .unwrap();

            // Expect initialized notification.
            let frame = stream.next().await.unwrap().unwrap();
            let text = frame.to_text().unwrap().to_string();
            let note: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                note.get("method").and_then(Value::as_str),
                Some("initialized")
            );
            let _ = initialized_tx.send(());

            // Keep the socket open until the client asserts Ready.
            let _ = shutdown_rx.await;
            let _ = sink.close().await;
        });

        let transport = UnixWsAppServerTransport::connect_with_runtime(
            sock,
            "0.146.1".to_string(),
            Some("/tmp/codex".into()),
        )
        .await
        .expect("connect");

        // Wait for Ready from the initialize path while the server stays up.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while transport.connection_state() != UpstreamConnectionState::Ready {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for Ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        tokio::time::timeout(Duration::from_secs(2), initialized_rx)
            .await
            .expect("initialized notification")
            .expect("initialized channel");

        let identity = transport.initialize().await.expect("identity");
        assert_eq!(identity.version, "0.146.1");
        assert!(transport.epoch() >= 1);
        assert_eq!(transport.connection_state(), UpstreamConnectionState::Ready);

        let _ = shutdown_tx.send(());
        server.await.unwrap();
        let _ = Arc::clone(&transport);
    }
}

#[cfg(not(unix))]
#[test]
fn unix_ws_initialize_round_trip_skipped_on_non_unix() {
    // Windows CI compiles the transport stub; live socket test runs on Linux CI.
}
