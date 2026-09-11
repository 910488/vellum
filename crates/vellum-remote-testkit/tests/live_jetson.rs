//! Isolated Jetson live smoke tests.
//!
//! These tests are ignored by default and require:
//!   VELLUM_LIVE_SMOKE=YES
//!   VELLUM_LIVE_SSH=tester@192.0.2.10   # or SSH config alias
//!   VELLUM_LIVE_BROKER_BIN=<aarch64 broker binary>
//!
//! Safety:
//! - never uses production ~/.codex runtime state
//! - never uses production app-server sockets
//! - only controls systemd units named vellum-*-smoke-<id>

#![cfg(feature = "live-smoke")]

use std::time::Duration;

use vellum_remote_protocol::{CommandStatus, ThreadRuntimeStatus, CURSOR_SCHEME, PROTOCOL_VERSION};
use vellum_remote_testkit::live::{
    isolation_guard_summary, resolve_local_broker_binary, JetsonLiveFixture, LiveBrokerClient,
};

fn live_enabled() -> bool {
    matches!(
        std::env::var("VELLUM_LIVE_SMOKE").as_deref(),
        Ok("1") | Ok("YES") | Ok("yes") | Ok("true") | Ok("TRUE")
    )
}

async fn bootstrap_fixture() -> JetsonLiveFixture {
    let mut fixture = JetsonLiveFixture::provision()
        .await
        .expect("provision isolated smoke fixture");
    let version = fixture.codex_version().await.expect("codex --version");
    fixture.trace.codex_version = Some(version.clone());
    assert!(
        version.contains("0.146.0"),
        "expected Jetson codex 0.146.0, got {version}"
    );

    let bin = resolve_local_broker_binary().expect("aarch64 broker binary");
    fixture
        .upload_broker_binary(&bin)
        .await
        .expect("upload broker");
    fixture
        .start_app_server()
        .await
        .expect("start isolated app-server");
    fixture.start_broker().await.expect("start isolated broker");
    fixture.open_tunnel().await.expect("open ssh tunnel");
    let _ = fixture.generate_schema_artifact().await;
    let _ = std::fs::write(
        fixture.local_artifact_dir.join("isolation.json"),
        serde_json::to_vec_pretty(&isolation_guard_summary(&fixture)).unwrap(),
    );
    fixture
}

async fn assert_no_gap_or_duplicate(client: &LiveBrokerClient) {
    let (dup, gap_count, gap) = client.stats().await;
    assert_eq!(dup, 0, "duplicate events observed: {dup}");
    assert_eq!(gap_count, 0, "gap events observed: {gap_count}");
    assert!(!gap, "gap flag set");
}

#[tokio::test]
#[ignore]
async fn jetson_live_ready() {
    if !live_enabled() {
        eprintln!("skip: VELLUM_LIVE_SMOKE not enabled");
        return;
    }
    let mut fixture = bootstrap_fixture().await;
    let client = fixture
        .connect_client("live-smoke-ready-device")
        .await
        .expect("connect client");
    let welcome = client.hello(None).await.expect("server.welcome");

    assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
    assert_eq!(welcome.cursor_scheme, CURSOR_SCHEME);
    assert!(
        welcome.upstream.epoch >= 1,
        "expected upstream epoch >= 1, got {}",
        welcome.upstream.epoch
    );
    fixture.trace.epoch_before = Some(welcome.upstream.epoch);
    if let Some(version) = welcome.upstream.codex_version.as_deref() {
        assert!(
            version.starts_with("0.146.0"),
            "welcome codex version should be pinned to 0.146.0, got {version}"
        );
    }

    let readyz = http_get_json(&format!(
        "http://127.0.0.1:{}/readyz",
        fixture.config.local_forward_port
    ))
    .await;
    assert_eq!(readyz["ready"], true, "readyz={readyz}");

    let _ = fixture.collect_logs().await;
    fixture.cleanup().await.ok();
    client.close().await;
}

#[tokio::test]
#[ignore]
async fn jetson_live_turn() {
    if !live_enabled() {
        return;
    }
    let mut fixture = bootstrap_fixture().await;
    let client = fixture
        .connect_client("live-smoke-turn-device")
        .await
        .expect("connect");
    let welcome = client.hello(None).await.expect("welcome");
    fixture.trace.epoch_before = Some(welcome.upstream.epoch);

    let start = client
        .thread_start(
            &fixture.workspace.display().to_string().replace('\\', "/"),
            None,
        )
        .await
        .expect("thread.start");
    let start = LiveBrokerClient::require_completed(start)
        .await
        .expect("thread.start completed");
    let thread_id = start
        .result
        .get("threadId")
        .and_then(|v| v.as_str())
        .expect("threadId")
        .to_string();
    fixture.trace.thread_id = Some(thread_id.clone());

    let _ = client
        .subscribe_thread(
            &thread_id,
            0,
            vellum_remote_protocol::SubscriptionMode::Writer,
        )
        .await
        .expect("subscribe");
    let acquire = client
        .writer_acquire(&thread_id)
        .await
        .expect("writer.acquire");
    LiveBrokerClient::require_completed(acquire)
        .await
        .expect("writer.acquire completed");

    let turn = client
        .turn_start(&thread_id, "Respond with exactly:\nVELLUM_SMOKE_OK")
        .await
        .expect("turn.start");
    LiveBrokerClient::require_completed(turn)
        .await
        .expect("turn.start accepted/completed");

    // Terminal may already be present in history buffer / future event stream.
    let (status, final_seq) = client
        .wait_for_terminal(Duration::from_secs(180))
        .await
        .expect("terminal state");
    assert_eq!(
        status,
        ThreadRuntimeStatus::Completed,
        "success smoke requires Completed, got {status:?}"
    );
    fixture.trace.turn_completed = true;
    fixture.trace.final_seq = Some(final_seq);
    assert_no_gap_or_duplicate(&client).await;
    let (dup, gap_count, _) = client.stats().await;
    fixture.trace.duplicate = dup > 0;
    fixture.trace.gap = gap_count > 0;

    let _ = fixture.collect_logs().await;
    fixture.cleanup().await.ok();
    client.close().await;
}

#[tokio::test]
#[ignore]
async fn jetson_live_detach_resume() {
    if !live_enabled() {
        return;
    }
    let mut fixture = bootstrap_fixture().await;
    let device = "live-smoke-detach-device";
    let client_a = fixture.connect_client(device).await.expect("client A");
    let _ = client_a.hello(None).await.expect("welcome A");

    let start = client_a
        .thread_start(
            &fixture.workspace.display().to_string().replace('\\', "/"),
            None,
        )
        .await
        .expect("thread.start");
    let start = LiveBrokerClient::require_completed(start)
        .await
        .expect("thread.start completed");
    let thread_id = start.result["threadId"].as_str().unwrap().to_string();
    fixture.trace.thread_id = Some(thread_id.clone());

    let _ = client_a
        .subscribe_thread(
            &thread_id,
            0,
            vellum_remote_protocol::SubscriptionMode::Writer,
        )
        .await;
    LiveBrokerClient::require_completed(client_a.writer_acquire(&thread_id).await.unwrap())
        .await
        .unwrap();

    let prompt = "Run ./smoke_delay.sh. Wait for it to finish. Then report its output exactly.";
    let turn = client_a
        .turn_start(&thread_id, prompt)
        .await
        .expect("turn.start");
    assert!(
        !matches!(turn.status, CommandStatus::Failed),
        "turn.start failed: {:?}",
        turn.error
    );

    // Wait until turn is visibly running, then drop client A only.
    let _ = client_a
        .wait_for_event(Duration::from_secs(60), |event| {
            event.method.contains("turn/started")
                || event.method.contains("item/")
                || event.method.contains("command")
        })
        .await;
    let disconnect_seq = client_a.last_ack_seq().await;
    fixture.trace.disconnect_seq = Some(disconnect_seq);
    assert!(disconnect_seq > 0, "expected progress before disconnect");
    // Abrupt drop models desktop crash / network loss, not WS close handshake.
    client_a.disconnect_abrupt().await;

    // No client for a bit while broker/app-server continue.
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Single recovery path for this Gate: hello without resume, then
    // thread.subscribe(lastAck) is authoritative. hello.resume is covered
    // separately (future jetson_live_hello_resume_hint).
    let client_b = fixture.connect_client(device).await.expect("client B");
    let welcome = client_b.hello(None).await.expect("welcome B");
    fixture.trace.epoch_after = Some(welcome.upstream.epoch);

    let recovered = client_b
        .recover_thread(
            &thread_id,
            disconnect_seq,
            vellum_remote_protocol::SubscriptionMode::Observer,
            Duration::from_secs(180),
        )
        .await
        .expect("recover after detach");

    fixture.trace.events_generated_while_detached = Some(recovered.replay.len() as u64);
    fixture.trace.final_seq = Some(recovered.final_seq);
    fixture.trace.turn_completed =
        matches!(recovered.terminal, Some(ThreadRuntimeStatus::Completed));
    assert!(
        recovered.final_seq > disconnect_seq,
        "final_seq {} should advance past disconnect_seq {}",
        recovered.final_seq,
        disconnect_seq
    );
    assert_eq!(
        recovered.terminal,
        Some(ThreadRuntimeStatus::Completed),
        "success smoke requires Completed recovery, got {:?}",
        recovered.terminal
    );
    assert_no_gap_or_duplicate(&client_b).await;
    let (dup, gap_count, _) = client_b.stats().await;
    fixture.trace.duplicate = dup > 0;
    fixture.trace.gap = gap_count > 0;
    fixture.trace.note(format!(
        "detach recovery via snapshot={} replay={} future={}",
        recovered.from_snapshot, recovered.from_replay, recovered.from_future_event
    ));

    let _ = fixture.collect_logs().await;
    fixture.cleanup().await.ok();
    client_b.close().await;
}

#[tokio::test]
#[ignore]
async fn jetson_live_broker_restart() {
    if !live_enabled() {
        return;
    }
    let mut fixture = bootstrap_fixture().await;
    let device = "live-smoke-restart-device";
    let client = fixture.connect_client(device).await.expect("client");
    let welcome = client.hello(None).await.expect("welcome");
    fixture.trace.epoch_before = Some(welcome.upstream.epoch);

    let start = client
        .thread_start(
            &fixture.workspace.display().to_string().replace('\\', "/"),
            None,
        )
        .await
        .expect("thread.start");
    let start = LiveBrokerClient::require_completed(start)
        .await
        .expect("thread.start completed");
    let thread_id = start.result["threadId"].as_str().unwrap().to_string();
    fixture.trace.thread_id = Some(thread_id.clone());
    let _ = client
        .subscribe_thread(
            &thread_id,
            0,
            vellum_remote_protocol::SubscriptionMode::Writer,
        )
        .await;
    LiveBrokerClient::require_completed(client.writer_acquire(&thread_id).await.unwrap())
        .await
        .unwrap();

    let prompt = "Run ./smoke_delay.sh. Wait for it to finish. Then report its output exactly.";
    let _ = client
        .turn_start(&thread_id, prompt)
        .await
        .expect("turn.start");
    let _ = client
        .wait_for_event(Duration::from_secs(60), |event| {
            event.method.contains("turn/started") || event.method.contains("item/")
        })
        .await;
    let before_seq = client.last_ack_seq().await;
    fixture.trace.disconnect_seq = Some(before_seq);
    // Abrupt drop before restart; must not deadlock on reader-held ACK sender.
    client.disconnect_abrupt().await;

    // Restart broker only; app-server unit stays up.
    fixture.restart_broker().await.expect("restart broker");
    tokio::time::sleep(Duration::from_secs(2)).await;
    fixture.open_tunnel().await.expect("reopen tunnel");

    // Same single recovery path as detach: hello(None) + subscribe(lastAck).
    let client2 = fixture.connect_client(device).await.expect("client2");
    let welcome2 = client2.hello(None).await.expect("welcome after restart");
    fixture.trace.epoch_after = Some(welcome2.upstream.epoch);

    let recovered = client2
        .recover_thread(
            &thread_id,
            before_seq,
            vellum_remote_protocol::SubscriptionMode::Observer,
            Duration::from_secs(180),
        )
        .await
        .expect("recover after broker restart");
    fixture.trace.turn_completed =
        matches!(recovered.terminal, Some(ThreadRuntimeStatus::Completed));
    fixture.trace.final_seq = Some(recovered.final_seq);
    assert!(
        recovered.final_seq > before_seq,
        "final_seq {} should advance past before_seq {}",
        recovered.final_seq,
        before_seq
    );
    assert_eq!(
        recovered.terminal,
        Some(ThreadRuntimeStatus::Completed),
        "success smoke requires Completed recovery after restart, got {:?}",
        recovered.terminal
    );
    assert_no_gap_or_duplicate(&client2).await;
    let (dup, gap_count, _) = client2.stats().await;
    fixture.trace.duplicate = dup > 0;
    fixture.trace.gap = gap_count > 0;

    let _ = fixture.collect_logs().await;
    fixture.cleanup().await.ok();
    client2.close().await;
}

async fn http_get_json(url: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let url = url::Url::parse(url).expect("url");
    let host = url.host_str().unwrap();
    let port = url.port_or_known_default().unwrap();
    let path = url.path().to_string();
    let mut stream = TcpStream::connect((host, port)).await.expect("tcp");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("{}");
    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"raw": body}))
}
