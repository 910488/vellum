use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Mutex;

use crate::protocol::{
    BindParams, BindResult, ControlFrame, HelloParams, HelloResult, StatusResult, TapLifecycle,
    TurnCancelParams, TurnCompletedEvent, TurnDeltaEvent, TurnOutcome, TurnStartParams,
    TurnStartResult, CONTROL_PROTOCOL_VERSION, EVENT_TURN_COMPLETED, EVENT_TURN_DELTA, METHOD_BIND,
    METHOD_HELLO, METHOD_STATUS, METHOD_TURN_CANCEL, METHOD_TURN_START,
};
use crate::ZcodeArtifact;

struct FakeState {
    artifact: ZcodeArtifact,
    sessions: Vec<String>,
    bindings: HashMap<String, String>,
    inflight: HashSet<String>,
}

pub async fn run_fake_tap(stream: DuplexStream, artifact: ZcodeArtifact, sessions: Vec<String>) {
    let (reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let state = Arc::new(Mutex::new(FakeState {
        artifact,
        sessions,
        bindings: HashMap::new(),
        inflight: HashSet::new(),
    }));
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let frame: ControlFrame = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        let Some(id) = frame.id.clone() else { continue };
        let method = frame.method.as_deref().unwrap_or("");
        let params = frame.params.unwrap_or(Value::Null);
        let reply = match method {
            METHOD_HELLO => hello(&state, id.clone(), params).await,
            METHOD_BIND => bind(&state, id.clone(), params).await,
            METHOD_TURN_START => turn_start(&state, Arc::clone(&writer), id.clone(), params).await,
            METHOD_TURN_CANCEL => turn_cancel(&state, id.clone(), params).await,
            METHOD_STATUS => status(&state, id.clone()).await,
            _ => ControlFrame::error(Some(id.clone()), "UNKNOWN_METHOD", method),
        };
        let mut out = writer.lock().await;
        let _ = out.write_all(&serde_json::to_vec(&reply).unwrap()).await;
        let _ = out.write_all(b"\n").await;
        let _ = out.flush().await;
    }
}

async fn hello(state: &Mutex<FakeState>, id: Value, params: Value) -> ControlFrame {
    let parsed: HelloParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => return ControlFrame::error(Some(id), "BAD_PARAMS", error.to_string()),
    };
    if parsed.protocol_version != CONTROL_PROTOCOL_VERSION {
        return ControlFrame::error(
            Some(id),
            "PROTOCOL_MISMATCH",
            format!("{}", parsed.protocol_version),
        );
    }
    let state = state.lock().await;
    if let Some(expected) = parsed.expected_cjs_sha256 {
        if expected != state.artifact.cjs_sha256 {
            return ControlFrame::error(
                Some(id),
                "ARTIFACT_MISMATCH",
                state.artifact.cjs_sha256.clone(),
            );
        }
    }
    ControlFrame::result(
        id,
        serde_json::to_value(HelloResult {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            artifact: state.artifact.clone(),
            pid: 1,
            inner_pid: Some(2),
            sessions: state.sessions.clone(),
            lifecycle: TapLifecycle::Ready,
        })
        .unwrap(),
    )
}

fn with_id(mut frame: ControlFrame, id: Value) -> ControlFrame {
    frame.id = Some(id);
    frame
}

async fn bind(state: &Mutex<FakeState>, id: Value, params: Value) -> ControlFrame {
    let parsed: BindParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => return ControlFrame::error(Some(id), "BAD_PARAMS", error.to_string()),
    };
    let mut state = state.lock().await;
    let session_id = match parsed.session_id {
        Some(session) => {
            if !state.sessions.contains(&session) {
                return ControlFrame::error(Some(id), "SESSION_NOT_FOUND", session);
            }
            if let Some(existing) = state.bindings.get(&parsed.vellum_thread_id) {
                if existing != &session {
                    return ControlFrame::error(Some(id), "THREAD_ALREADY_BOUND", existing.clone());
                }
            }
            session
        }
        None => match state.sessions.first() {
            Some(session) => session.clone(),
            None => return ControlFrame::error(Some(id), "SESSION_NOT_FOUND", "no live session"),
        },
    };
    state
        .bindings
        .insert(parsed.vellum_thread_id.clone(), session_id.clone());
    ControlFrame::result(
        id,
        serde_json::to_value(BindResult {
            vellum_thread_id: parsed.vellum_thread_id,
            session_id,
        })
        .unwrap(),
    )
}

async fn turn_start(
    state: &Mutex<FakeState>,
    writer: Arc<Mutex<impl tokio::io::AsyncWrite + Unpin + Send + 'static>>,
    id: Value,
    params: Value,
) -> ControlFrame {
    let parsed: TurnStartParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => {
            return with_id(
                ControlFrame::error(None, "BAD_PARAMS", error.to_string()),
                id,
            )
        }
    };
    if parsed.content.trim().is_empty() {
        return with_id(
            ControlFrame::error(None, "BAD_PARAMS", "content is required"),
            id,
        );
    }
    let mut state = state.lock().await;
    if !state.inflight.insert(parsed.vellum_turn_id.clone()) {
        return with_id(
            ControlFrame::error(None, "DUPLICATE_TURN", parsed.vellum_turn_id),
            id,
        );
    }
    let session_id = state
        .bindings
        .get(&parsed.vellum_thread_id)
        .cloned()
        .or_else(|| state.sessions.first().cloned());
    let Some(session_id) = session_id else {
        state.inflight.remove(&parsed.vellum_turn_id);
        return with_id(
            ControlFrame::error(None, "SESSION_NOT_FOUND", parsed.vellum_thread_id),
            id,
        );
    };
    let result = TurnStartResult {
        vellum_turn_id: parsed.vellum_turn_id.clone(),
        session_id: session_id.clone(),
        native_request_id: format!("900001-{}", parsed.vellum_turn_id),
        input_id: format!("input-{}", parsed.vellum_turn_id),
    };
    let turn_id = parsed.vellum_turn_id.clone();
    let content = parsed.content.clone();
    drop(state);
    let writer_events = writer;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let delta = ControlFrame::event(
            EVENT_TURN_DELTA,
            serde_json::to_value(TurnDeltaEvent {
                vellum_turn_id: turn_id.clone(),
                session_id: session_id.clone(),
                native_turn_id: Some("turn_fake".into()),
                text: Some(content),
            })
            .unwrap(),
        );
        let done = ControlFrame::event(
            EVENT_TURN_COMPLETED,
            serde_json::to_value(TurnCompletedEvent {
                vellum_turn_id: turn_id,
                session_id,
                outcome: TurnOutcome::Completed,
                provider_id: None,
                model_id: None,
                error_code: None,
                error_message: None,
            })
            .unwrap(),
        );
        let mut out = writer_events.lock().await;
        for frame in [delta, done] {
            let _ = out.write_all(&serde_json::to_vec(&frame).unwrap()).await;
            let _ = out.write_all(b"\n").await;
        }
        let _ = out.flush().await;
    });
    ControlFrame::result(id, serde_json::to_value(result).unwrap())
}

async fn turn_cancel(state: &Mutex<FakeState>, id: Value, params: Value) -> ControlFrame {
    let parsed: TurnCancelParams = match serde_json::from_value(params) {
        Ok(parsed) => parsed,
        Err(error) => return ControlFrame::error(Some(id), "BAD_PARAMS", error.to_string()),
    };
    let mut state = state.lock().await;
    if !state.inflight.remove(&parsed.vellum_turn_id) {
        return ControlFrame::error(Some(id), "TURN_NOT_FOUND", parsed.vellum_turn_id);
    }
    ControlFrame::result(id, json!({ "cancelled": true }))
}

async fn status(state: &Mutex<FakeState>, id: Value) -> ControlFrame {
    let state = state.lock().await;
    ControlFrame::result(
        id,
        serde_json::to_value(StatusResult {
            lifecycle: TapLifecycle::Ready,
            sessions: state.sessions.clone(),
            inflight_turns: state.inflight.iter().cloned().collect(),
        })
        .unwrap(),
    )
}
