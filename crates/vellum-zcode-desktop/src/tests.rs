use std::time::Duration;

use tokio::io::duplex;

use super::fake::run_fake_tap;
use super::*;
use crate::channel::ControlClient;
use crate::protocol::{BindParams, TurnCancelParams, TurnStartParams};

fn artifact(sha: &str) -> ZcodeArtifact {
    ZcodeArtifact {
        cjs_sha256: sha.into(),
        product_version: Some("3.10.1".into()),
    }
}

async fn connected(sha: &str, sessions: Vec<&str>) -> ControlClient {
    let (client_io, server_io) = duplex(64 * 1024);
    let artifact = artifact(sha);
    let sessions = sessions.into_iter().map(str::to_owned).collect();
    tokio::spawn(run_fake_tap(server_io, artifact, sessions));
    let client = ControlClient::from_duplex(client_io);
    client
        .hello(Some(&ArtifactPin {
            cjs_sha256: sha.to_owned(),
        }))
        .await
        .expect("hello");
    client
}

#[tokio::test]
async fn hello_rejects_artifact_mismatch() {
    let (client_io, server_io) = duplex(64 * 1024);
    tokio::spawn(run_fake_tap(
        server_io,
        artifact("aaa"),
        vec!["sess_live".into()],
    ));
    let client = ControlClient::from_duplex(client_io);
    let error = client
        .hello(Some(&ArtifactPin {
            cjs_sha256: "bbb".into(),
        }))
        .await
        .expect_err("pin mismatch");
    assert!(matches!(error, ZcodeDesktopError::ArtifactMismatch { .. }));
}

#[tokio::test]
async fn bind_and_turn_emits_delta_then_completed() {
    let client = connected("sha-1", vec!["sess_live"]).await;
    let bound = client
        .bind(BindParams {
            vellum_thread_id: "ui-1".into(),
            session_id: Some("sess_live".into()),
        })
        .await
        .unwrap();
    assert_eq!(bound.session_id, "sess_live");
    let mut events = client.subscribe();
    let started = client
        .start_turn(TurnStartParams {
            vellum_thread_id: "ui-1".into(),
            vellum_turn_id: "turn-1".into(),
            content: "Reply with PONG".into(),
            timeout_ms: Some(5_000),
        })
        .await
        .unwrap();
    assert_eq!(started.session_id, "sess_live");
    let mut saw_delta = false;
    let mut saw_done = false;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            ControlEvent::TurnDelta(delta) => {
                assert_eq!(delta.vellum_turn_id, "turn-1");
                assert_eq!(delta.text.as_deref(), Some("Reply with PONG"));
                saw_delta = true;
            }
            ControlEvent::TurnCompleted(done) => {
                assert_eq!(done.outcome, TurnOutcome::Completed);
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_delta && saw_done);
}

#[tokio::test]
async fn duplicate_turn_is_rejected() {
    let client = connected("sha-1", vec!["sess_live"]).await;
    client
        .bind(BindParams {
            vellum_thread_id: "ui-1".into(),
            session_id: None,
        })
        .await
        .unwrap();
    let params = TurnStartParams {
        vellum_thread_id: "ui-1".into(),
        vellum_turn_id: "turn-dup".into(),
        content: "one".into(),
        timeout_ms: None,
    };
    client.start_turn(params.clone()).await.unwrap();
    let error = client.start_turn(params).await.expect_err("duplicate");
    assert!(matches!(error, ZcodeDesktopError::DuplicateTurn(_)));
}

#[tokio::test]
async fn cancel_unknown_turn_fails_closed() {
    let client = connected("sha-1", vec!["sess_live"]).await;
    let error = client
        .cancel_turn(TurnCancelParams {
            vellum_thread_id: "ui-1".into(),
            vellum_turn_id: "missing".into(),
        })
        .await
        .expect_err("missing turn");
    assert!(matches!(error, ZcodeDesktopError::TurnNotFound(_)));
}

#[tokio::test]
async fn binding_store_pins_thread_to_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = ZcodeBindingStore::open(dir.path().join("bind.sqlite")).unwrap();
    let now = chrono::Utc::now();
    let binding = ZcodeThreadBinding {
        vellum_thread_id: "ui-9".into(),
        zcode_session_id: "sess_fixed".into(),
        workspace: "C:/work".into(),
        artifact_sha256: "sha-1".into(),
        runtime_instance_id: "42".into(),
        created_at: now,
        last_seen_at: now,
    };
    store.upsert(&binding).unwrap();
    let loaded = store.get("ui-9").unwrap().unwrap();
    assert_eq!(loaded.zcode_session_id, "sess_fixed");
    store.remove("ui-9").unwrap();
    assert!(store.get("ui-9").unwrap().is_none());
}

#[test]
fn fingerprint_is_stable() {
    assert_eq!(
        fingerprint_bytes(b"zcode.cjs"),
        fingerprint_bytes(b"zcode.cjs")
    );
    assert_ne!(fingerprint_bytes(b"a"), fingerprint_bytes(b"b"));
}

#[test]
fn newest_listen_record_is_selected() {
    let dir = tempfile::tempdir().unwrap();
    let older = TapListenRecord {
        schema_version: 1,
        protocol_version: CONTROL_PROTOCOL_VERSION,
        pid: 1,
        inner_pid: None,
        pipe: r"\\.\pipe\old".into(),
        artifact: artifact("aaa"),
        started_at: "2026-01-01T00:00:00Z".into(),
    };
    let newer = TapListenRecord {
        pid: 2,
        pipe: r"\\.\pipe\new".into(),
        started_at: "2026-08-31T00:00:00Z".into(),
        ..older.clone()
    };
    std::fs::write(
        dir.path().join("tap-1.listen.json"),
        serde_json::to_string(&older).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("tap-2.listen.json"),
        serde_json::to_string(&newer).unwrap(),
    )
    .unwrap();
    let found = newest(dir.path()).unwrap().unwrap();
    assert_eq!(found.pid, 2);
}
