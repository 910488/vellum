//! WebSocket Responses transport.
//!
//! Official Responses is a native, long-lived, bidirectional WebSocket
//! tunnel. Every `response.create` re-resolves the catalog model and route
//! on a fresh snapshot (plan: "WebSocket per-turn routing"): a compatible
//! Official turn reuses one upstream socket with only the model swapped per
//! turn, and everything else relays through the portable HTTP adapters.

use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream};

type OfficialUpstreamSocket =
    tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const OFFICIAL_CANCEL_DRAIN_TIMEOUT_MS: u64 = 250;

use crate::body::MAX_REQUEST_BODY_BYTES;
use crate::diagnostics::{DiagnosticEvent, WebSocketClosed, WebSocketOpened, WebSocketTransport};
use crate::error::RuntimeError;
use crate::exec::{
    OfficialWebSocketConnectionKey, OfficialWebSocketPlan, ProxyRuntime, RuntimeResponse,
    WebSocketTurnDispatch, WebSocketTurnPlan,
};
use crate::request::{IncomingAuthContext, RuntimeEndpoint, RuntimeRequest};
use crate::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use crate::state::ProxyRuntimeState;
use crate::usage::UsageRecord;

/// Upgrade handler for `GET /v1/responses` and `GET /responses`.
pub async fn responses_websocket<S: ProxyRuntimeState>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<S>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    ws.max_message_size(MAX_REQUEST_BODY_BYTES)
        .max_frame_size(MAX_REQUEST_BODY_BYTES)
        .on_upgrade(move |socket| bridge(socket, state, headers))
}

struct DeferredOfficialTurn {
    plan: OfficialWebSocketPlan,
    request: Value,
    runtime_request: RuntimeRequest,
    request_id: String,
    execution_id: String,
}

#[allow(clippy::large_enum_variant)]
enum WebSocketWork {
    New(Value),
    Deferred(DeferredOfficialTurn),
}

fn managed_account_changed(
    current: &OfficialWebSocketConnectionKey,
    next: &OfficialWebSocketConnectionKey,
) -> bool {
    matches!(
        (&current.auth_posture, &next.auth_posture),
        (
            crate::exec::OfficialWebSocketAuthPosture::Managed {
                account_id: Some(current),
            },
            crate::exec::OfficialWebSocketAuthPosture::Managed {
                account_id: Some(next),
            },
        ) if current != next
    )
}

/// Per-turn WebSocket dispatcher. Every `response.create` re-resolves the
/// catalog model and route against a fresh snapshot: an Official turn on a
/// route compatible with the currently open segment (same upstream URL and
/// auth posture) reuses that one native upstream socket, with only the
/// upstream model swapped per turn; everything else runs through the
/// portable HTTP adapters. Encrypted Official reasoning and
/// `previous_response_id` never cross a provider switch: a portable turn
/// only ever sees the sanitized/hydrated body the shared execute() path
/// already produces, and a fresh Official segment starts from a clean
/// handshake with no leftover state from whatever ran before it. A
/// `response.create` that would move off the currently open Official
/// segment's route while that segment still has turns in flight is
/// rejected (fail closed) instead of being silently reordered onto a
/// different transport; same-route repeats queue on the existing segment or
/// (for portable turns) behind the in-flight one exactly as before. A turn
/// that only changes the managed Official account is retained as an immutable
/// deferred plan and sent FIFO after the old segment drains.
async fn bridge<S: ProxyRuntimeState>(mut client: WebSocket, state: Arc<S>, headers: HeaderMap) {
    let connection_id = format!("conn_{}", ulid::Ulid::new());
    let connection_started = std::time::Instant::now();
    let runtime = state.proxy_runtime();
    let mut official: Option<OfficialSegment> = None;
    let mut queued: VecDeque<Value> = VecDeque::new();
    let mut deferred_official: VecDeque<DeferredOfficialTurn> = VecDeque::new();
    let mut deferred_account_handoffs: VecDeque<Value> = VecDeque::new();
    let mut cancelled_response_ids: VecDeque<String> = VecDeque::new();
    let mut turns = 0_u64;
    let mut frames_in = 0_u64;
    let mut frames_out = 0_u64;
    let mut opened = false;
    // Every exit path below sets `reason` immediately before breaking; the
    // initializer only documents the fallback meaning and is intentionally
    // never read as-is.
    #[allow(unused_assignments)]
    let mut reason = "client closed".to_string();

    'connection: loop {
        let work = loop {
            if official
                .as_ref()
                .is_none_or(|segment| segment.pending.is_empty())
            {
                if let Some(request) = deferred_account_handoffs.pop_front() {
                    break WebSocketWork::New(request);
                }
                if let Some(turn) = deferred_official.pop_front() {
                    break WebSocketWork::Deferred(turn);
                }
            }
            if let Some(value) = queued.pop_front() {
                break WebSocketWork::New(value);
            }
            tokio::select! {
                client_message = client.recv() => {
                    match client_message {
                        None | Some(Err(_)) => {
                            reason = "client closed".into();
                            break 'connection;
                        }
                        Some(Ok(AxumWsMessage::Close(_))) => {
                            reason = "client closed".into();
                            break 'connection;
                        }
                        Some(Ok(AxumWsMessage::Ping(payload))) => {
                            let _ = client.send(AxumWsMessage::Pong(payload)).await;
                        }
                        Some(Ok(AxumWsMessage::Pong(_))) => {}
                        Some(Ok(message)) => {
                            frames_in += 1;
                            match websocket_request_value(&message) {
                                Ok(value) => break WebSocketWork::New(value),
                                Err(error) => {
                                    if send_websocket_failure(&mut client, &error).await {
                                        frames_out += 1;
                                    }
                                    reason = error;
                                    break 'connection;
                                }
                            }
                        }
                    }
                }
                official_event = next_official_event(&mut official) => {
                    match official_event {
                        OfficialEvent::TreeCancelled => {
                            handle_official_tree_cancel(
                                runtime.as_ref(),
                                &mut official,
                                &connection_id,
                            )
                            .await;
                        }
                        OfficialEvent::CancelledTombstoneTimeout => {
                            handle_official_cancel_timeout(
                                runtime.as_ref(),
                                &mut official,
                                &connection_id,
                            )
                            .await;
                        }
                        OfficialEvent::Retry => {}
                        OfficialEvent::Frame(upstream_message) => {
                            if !handle_official_upstream_frame(
                                &mut client,
                                runtime.as_ref(),
                                &mut official,
                                &connection_id,
                                upstream_message,
                                &mut frames_out,
                            )
                            .await
                            {
                                reason = "upstream closed".into();
                                break 'connection;
                            }
                        }
                    }
                }
            }
        };

        let (request, preplanned_runtime_request, preplanned_plan, preplanned_execution_id) =
            match work {
                WebSocketWork::New(request) => (request, None, None, None),
                WebSocketWork::Deferred(turn) => (
                    turn.request,
                    Some(turn.runtime_request),
                    Some(turn.plan),
                    Some(turn.execution_id),
                ),
            };

        // A bare Responses body carried over the WebSocket upgrade has no
        // `type` discriminator of its own -- that tag is a WebSocket-only
        // envelope convention, not part of the Responses API request shape.
        // The per-turn dispatch loop must accept an untagged frame as an
        // implicit `response.create` (matching the pre-per-turn-routing
        // bridge, which treated every non-`response.cancel` frame as a turn
        // to execute) or it silently drops the client's very first request
        // whenever the client doesn't tag it.
        match request.get("type").and_then(Value::as_str) {
            Some("response.create") | None => {
                turns += 1;
                let was_deferred = preplanned_plan.is_some();
                let mut runtime_request = preplanned_runtime_request.clone().unwrap_or_else(|| {
                    runtime_request_from_body(&state, &headers, &request, &connection_id)
                });
                let request_id = runtime_request.metadata.request_id.clone();
                let turn_plan = if let Some(plan) = preplanned_plan {
                    WebSocketTurnPlan {
                        route_id: plan.route_id.clone(),
                        dispatch: WebSocketTurnDispatch::OfficialNative(plan),
                    }
                } else {
                    match runtime.prepare_websocket_turn(&runtime_request).await {
                        Ok(plan) => plan,
                        Err(error) => {
                            if send_runtime_failure(&mut client, &error).await {
                                frames_out += 1;
                            }
                            continue;
                        }
                    }
                };
                let WebSocketTurnPlan {
                    route_id: next_route_id,
                    dispatch: mut turn_dispatch,
                } = turn_plan;
                let live_account_handoff = matches!(
                    &turn_dispatch,
                    WebSocketTurnDispatch::OfficialNative(plan)
                        if request.get("previous_response_id").and_then(Value::as_str).is_some()
                            && official.as_ref().is_some_and(|segment| {
                                managed_account_changed(
                                    &segment.connection_key,
                                    &plan.connection_key,
                                )
                            })
                );
                if live_account_handoff {
                    if official
                        .as_ref()
                        .is_some_and(|segment| !segment.pending.is_empty())
                    {
                        deferred_account_handoffs.push_back(request);
                        continue;
                    }
                    runtime_request.metadata.force_portable_official_handoff = true;
                    turn_dispatch = WebSocketTurnDispatch::PortableReplay {
                        force_official_handoff: true,
                    };
                }
                match turn_dispatch {
                    WebSocketTurnDispatch::OfficialNative(official_plan) => {
                        // Minted before Point B, not derived from
                        // `request_id`: this is the key the shared cancel
                        // registry uses for this turn, so a third-party
                        // parent/child can bind to (and fan a cancel into) an
                        // Official-native turn the same way it already does
                        // for portable turns.
                        let execution_id = preplanned_execution_id
                            .clone()
                            .unwrap_or_else(crate::exec::new_execution_id);
                        // Point B: capture this turn's native spawn tool
                        // calls for subagent attribution, regardless of
                        // whether it reuses the open segment or opens a
                        // fresh one. `true` means this turn confidently
                        // linked to a parent that was already cancelled --
                        // it must never reach upstream (parity with the
                        // portable/HTTP path's `record_subagent_child_turn`
                        // check).
                        let aborted_by_parent = !was_deferred
                            && runtime.record_official_websocket_child_turn(
                                &official_plan,
                                &runtime_request,
                                &execution_id,
                            );
                        if aborted_by_parent {
                            runtime.finish_terminal_once(
                                &execution_id,
                                &official_plan.route_id,
                                &official_plan.provider_name,
                                &official_plan.upstream_model,
                                &request_id,
                                Some(&connection_id),
                                0,
                                None,
                                None,
                                None,
                                "client_cancel",
                                Some("client_cancel"),
                                Some("cancelled by parent"),
                                None,
                            );
                            if send_websocket_failure(&mut client, "request cancelled").await {
                                frames_out += 1;
                            }
                            continue;
                        }
                        let conversation_key = runtime
                            .resolve_request_conversation_key(&runtime_request)
                            .key;
                        let reuse = official.as_ref().is_some_and(|segment| {
                            segment.connection_key == official_plan.connection_key
                        });
                        if reuse {
                            let segment = official.as_mut().expect("checked Some above");
                            match send_official_turn(
                                &mut client,
                                runtime.as_ref(),
                                segment,
                                official_plan,
                                request,
                                request_id,
                                execution_id,
                                conversation_key,
                                &connection_id,
                            )
                            .await
                            {
                                SendOfficialTurnOutcome::Sent
                                | SendOfficialTurnOutcome::AlreadyCancelled => {}
                                SendOfficialTurnOutcome::UpstreamSendFailed => {
                                    reason = "upstream send failed".into();
                                    break;
                                }
                            }
                        } else if official
                            .as_ref()
                            .is_some_and(|segment| !segment.pending.is_empty())
                        {
                            // Preserve the resolved account/token/revision
                            // snapshot. A later account switch must not
                            // rewrite this queued turn, and it is only sent
                            // after the current segment has drained.
                            deferred_official.push_back(DeferredOfficialTurn {
                                plan: official_plan,
                                request,
                                runtime_request,
                                request_id,
                                execution_id,
                            });
                        } else {
                            close_official_segment(&mut official).await;
                            let mut official_plan = official_plan;
                            match open_official_segment(
                                &mut client,
                                runtime.as_ref(),
                                &headers,
                                &connection_id,
                                &request_id,
                                &execution_id,
                                &mut official_plan,
                            )
                            .await
                            {
                                Some(mut segment) => {
                                    if !opened {
                                        opened = true;
                                        runtime.record_diagnostic(
                                            DiagnosticEvent::WebSocketOpened(WebSocketOpened {
                                                connection_id: connection_id.clone(),
                                                transport: WebSocketTransport::Official,
                                                upstream_host: websocket_upstream_host(
                                                    &official_plan.upstream_url,
                                                ),
                                            }),
                                        );
                                    }
                                    let sent = send_official_turn(
                                        &mut client,
                                        runtime.as_ref(),
                                        &mut segment,
                                        official_plan,
                                        request,
                                        request_id,
                                        execution_id,
                                        conversation_key,
                                        &connection_id,
                                    )
                                    .await;
                                    official = Some(segment);
                                    match sent {
                                        SendOfficialTurnOutcome::Sent
                                        | SendOfficialTurnOutcome::AlreadyCancelled => {}
                                        SendOfficialTurnOutcome::UpstreamSendFailed => {
                                            reason = "upstream send failed".into();
                                            break;
                                        }
                                    }
                                }
                                None => {
                                    reason = "official handshake failed".into();
                                    break;
                                }
                            }
                        }
                    }
                    WebSocketTurnDispatch::PortableReplay {
                        force_official_handoff,
                    } => {
                        if official
                            .as_ref()
                            .is_some_and(|segment| !segment.pending.is_empty())
                        {
                            if send_runtime_failure(
                                &mut client,
                                &RuntimeError::UnsupportedMode(format!(
                                    "cannot switch WebSocket route to `{next_route_id}` while a previous turn is still in flight"
                                )),
                            )
                            .await
                            {
                                frames_out += 1;
                            }
                            continue;
                        }
                        // Drained (or never opened): safe to close before the
                        // portable turn runs. Official's encrypted reasoning
                        // never reaches the portable adapters — the shared
                        // execute() path only hydrates the sanitized/portable
                        // history for this route.
                        close_official_segment(&mut official).await;
                        if !opened {
                            opened = true;
                            runtime.record_diagnostic(DiagnosticEvent::WebSocketOpened(
                                WebSocketOpened {
                                    connection_id: connection_id.clone(),
                                    transport: WebSocketTransport::HttpBridge,
                                    upstream_host: "http-bridge".into(),
                                },
                            ));
                        }
                        match execute_portable_turn(
                            &mut client,
                            &state,
                            runtime.as_ref(),
                            &headers,
                            request,
                            &connection_id,
                            &mut queued,
                            &mut frames_out,
                            force_official_handoff,
                        )
                        .await
                        {
                            TurnOutcome::Continue => {}
                            TurnOutcome::Stop(stop_reason) => {
                                reason = stop_reason;
                                break;
                            }
                        }
                    }
                }
            }
            Some("response.cancel") => {
                let requested_response_id = request.get("response_id").and_then(Value::as_str);
                let has_live_pending = official
                    .as_ref()
                    .is_some_and(|segment| segment.pending.iter().any(|turn| !turn.recorded));
                let duplicate_cancel = official.as_ref().is_some_and(|segment| {
                    requested_response_id.is_some_and(|response_id| {
                        cancelled_response_ids
                            .iter()
                            .any(|cancelled| cancelled == response_id)
                    }) || segment.pending.iter().any(|turn| {
                        turn.recorded
                            && requested_response_id.is_none()
                            && !has_live_pending
                            && deferred_official.is_empty()
                    })
                });
                if duplicate_cancel {
                    continue;
                }
                if has_live_pending {
                    match official.as_mut() {
                        Some(segment) => {
                            let cancelled_execution_id = if let Some(turn) =
                                segment.pending.iter_mut().find(|turn| !turn.recorded)
                            {
                                if let Some(response_id) = requested_response_id {
                                    remember_cancelled_response_id(
                                        &mut cancelled_response_ids,
                                        response_id,
                                    );
                                }
                                turn.cancelled_at = Some(std::time::Instant::now());
                                record_official_pending_outcome(
                                    runtime.as_ref(),
                                    turn,
                                    &connection_id,
                                    "client_cancel",
                                    Some("client_cancel"),
                                    Some("request cancelled"),
                                    &segment.stages,
                                );
                                // Keep the cancelled turn at the front until
                                // the upstream terminal frame is consumed.
                                // Otherwise that late frame can be attributed
                                // to the next pending turn on this segment.
                                Some(turn.execution_id.clone())
                            } else {
                                None
                            };
                            if let Some(execution_id) = cancelled_execution_id {
                                // Fan the cancel out to every confidently bound
                                // child of this Official-native turn (third-party or
                                // another Official turn), matching the fan-out the
                                // portable/HTTP bridge already does on cancel. Without
                                // this, an Official parent's `response.cancel` only
                                // ever stopped the Official leg itself.
                                runtime.cancel_request_tree(&execution_id);
                                runtime.unregister_request_cancel(&execution_id);
                            }
                            if send_official_upstream(&mut segment.write, text_frame(&request))
                                .await
                                .is_err()
                            {
                                reason = "upstream send failed".into();
                                break;
                            }
                        }
                        None => {
                            if send_websocket_failure(&mut client, "request cancelled").await {
                                frames_out += 1;
                            }
                            reason = "client cancelled".into();
                            break;
                        }
                    }
                } else if let Some(turn) = deferred_official.pop_front() {
                    finish_deferred_official_outcome(
                        runtime.as_ref(),
                        &turn,
                        &connection_id,
                        "client_cancel",
                        Some("client_cancel"),
                        Some("queued Official turn cancelled before upstream send"),
                    );
                    if send_websocket_failure(&mut client, "request cancelled").await {
                        frames_out += 1;
                    }
                    continue;
                } else {
                    match official.as_mut() {
                        Some(segment) => {
                            if send_official_upstream(&mut segment.write, text_frame(&request))
                                .await
                                .is_err()
                            {
                                reason = "upstream send failed".into();
                                break;
                            }
                        }
                        None => {
                            if send_websocket_failure(&mut client, "request cancelled").await {
                                frames_out += 1;
                            }
                            reason = "client cancelled".into();
                            break;
                        }
                    }
                }
            }
            _ => {
                // Codex's WebSocket protocol only sends response.create and
                // response.cancel; anything else is a forward-compatible
                // control frame. On an open Official segment it is relayed
                // upstream unchanged (native passthrough); with no segment
                // open there is nowhere to route it, so it is ignored.
                if let Some(segment) = official.as_mut() {
                    if send_official_upstream(&mut segment.write, text_frame(&request))
                        .await
                        .is_err()
                    {
                        reason = "upstream send failed".into();
                        break;
                    }
                }
            }
        }
    }

    for turn in deferred_official.drain(..) {
        finish_deferred_official_outcome(
            runtime.as_ref(),
            &turn,
            &connection_id,
            "client_disconnect",
            Some("client_disconnect"),
            Some("queued Official turn discarded when the client disconnected"),
        );
    }
    if let Some(mut segment) = official.take() {
        segment
            .stages
            .terminal_ms
            .get_or_insert(segment.started.elapsed().as_millis() as u64);
        let (outcome, category) = classify_official_close_reason(&reason);
        flush_official_pending(
            runtime.as_ref(),
            &mut segment.pending,
            &connection_id,
            outcome,
            category,
            Some(&reason),
            &segment.stages,
        );
        let _ = segment.write.close().await;
    }
    runtime.record_diagnostic(DiagnosticEvent::WebSocketClosed(WebSocketClosed {
        connection_id,
        turns,
        frames_in,
        frames_out,
        duration_ms: connection_started.elapsed().as_millis() as u64,
        reason,
    }));
}

/// One open native Official Responses WebSocket segment. `pending` is the
/// FIFO of turns forwarded on it that have not yet reached a terminal event;
/// Official natively pipelines multiple in-flight turns on one socket, so
/// completions are matched front-to-back.
struct OfficialSegment {
    connection_key: OfficialWebSocketConnectionKey,
    write: futures_util::stream::SplitSink<OfficialUpstreamSocket, TungsteniteMessage>,
    read: futures_util::stream::SplitStream<OfficialUpstreamSocket>,
    pending: VecDeque<OfficialPendingTurn>,
    stages: OfficialStageTimes,
    started: std::time::Instant,
}

enum TurnOutcome {
    Continue,
    Stop(String),
}

/// The result of racing an upstream read against every pending turn's
/// cancel-registry watch (see [`next_official_event`]).
enum OfficialEvent {
    Frame(Option<Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>),
    /// A parent-tree cancel landed on at least one pending turn on this
    /// segment, from *another* connection's `cancel_request_tree` call.
    TreeCancelled,
    /// A client-cancelled turn never received a terminal frame within the
    /// bounded drain window; the old segment must be closed so deferred turns
    /// cannot remain blocked forever on a silent upstream.
    CancelledTombstoneTimeout,
    /// A cancel watcher closed without reporting a cancellation; rebuild the
    /// race so a tombstone deadline remains armed.
    Retry,
}

/// Wait for either the next upstream frame or a parent-tree cancel landing on
/// one of this segment's pending turns. Official has no per-turn wire-level
/// cancel address and no server push for a cancellation that originates on a
/// different connection, so this is the only proactive way an in-flight
/// child ever learns its parent was cancelled: checking only when a frame
/// happens to arrive (the previous design) never fires if the provider
/// stops sending frames, which is exactly the case the 250ms termination
/// deadline has to cover.
async fn next_official_event(official: &mut Option<OfficialSegment>) -> OfficialEvent {
    let Some(segment) = official.as_mut() else {
        return std::future::pending().await;
    };
    if segment
        .pending
        .iter()
        .any(|turn| turn.cancel_rx.as_ref().is_some_and(|rx| *rx.borrow()))
    {
        return OfficialEvent::TreeCancelled;
    }
    let cancel_deadline = segment
        .pending
        .iter()
        .filter_map(|turn| {
            turn.cancelled_at.map(|cancelled_at| {
                cancelled_at + std::time::Duration::from_millis(OFFICIAL_CANCEL_DRAIN_TIMEOUT_MS)
            })
        })
        .min();
    let watchers: Vec<_> = segment
        .pending
        .iter_mut()
        .filter(|turn| !turn.recorded)
        .filter_map(|turn| turn.cancel_rx.as_mut())
        .map(|rx| Box::pin(rx.changed()))
        .collect();
    let wait = cancel_deadline
        .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
    if wait.is_none() && watchers.is_empty() {
        return OfficialEvent::Frame(segment.read.next().await);
    }
    if let Some(wait) = wait {
        if watchers.is_empty() {
            return tokio::select! {
                frame = segment.read.next() => OfficialEvent::Frame(frame),
                _ = tokio::time::sleep(wait) => OfficialEvent::CancelledTombstoneTimeout,
            };
        }
        tokio::select! {
            frame = segment.read.next() => OfficialEvent::Frame(frame),
            _ = tokio::time::sleep(wait) => OfficialEvent::CancelledTombstoneTimeout,
            (changed, _, _) = futures_util::future::select_all(watchers) => {
                if changed.is_ok() {
                    OfficialEvent::TreeCancelled
                } else {
                    OfficialEvent::Retry
                }
            }
        }
    } else {
        tokio::select! {
            frame = segment.read.next() => OfficialEvent::Frame(frame),
            (changed, _, _) = futures_util::future::select_all(watchers) => {
                if changed.is_ok() {
                    OfficialEvent::TreeCancelled
                } else {
                    OfficialEvent::Retry
                }
            }
        }
    }
}

/// Close the segment and record every still-pending turn's terminal outcome:
/// the turn(s) whose own cancel watch fired get `client_cancel`; any other
/// turns queued on the same segment get `client_disconnect` categorized as
/// `segment_closed_by_tree_cancel`, since Official has no per-turn cancel
/// address to target narrowly and the whole segment is torn down instead.
/// The downstream client connection itself stays open -- the next
/// `response.create` opens a fresh segment.
async fn handle_official_tree_cancel(
    runtime: &ProxyRuntime,
    official: &mut Option<OfficialSegment>,
    connection_id: &str,
) {
    if let Some(segment) = official.as_mut() {
        segment
            .stages
            .terminal_ms
            .get_or_insert(segment.started.elapsed().as_millis() as u64);
        let stages = segment.stages.clone();
        for turn in segment.pending.iter_mut() {
            let cancelled = turn.cancel_rx.as_ref().is_some_and(|rx| *rx.borrow());
            if cancelled {
                record_official_pending_outcome(
                    runtime,
                    turn,
                    connection_id,
                    "client_cancel",
                    Some("client_cancel"),
                    Some("cancelled by parent tree"),
                    &stages,
                );
            } else {
                record_official_pending_outcome(
                    runtime,
                    turn,
                    connection_id,
                    "client_disconnect",
                    Some("segment_closed_by_tree_cancel"),
                    Some(
                        "Official segment closed after a sibling turn's parent tree was cancelled",
                    ),
                    &stages,
                );
            }
        }
    }
    close_official_segment(official).await;
}

/// A client cancel is terminal locally, but a native upstream may never send
/// the corresponding terminal frame. Do not let that silent socket block
/// account-switched deferred turns forever: close the old segment after the
/// bounded drain window and fail any other still-pending old-segment turns
/// closed. The cancelled tombstone was already recorded as `client_cancel`.
async fn handle_official_cancel_timeout(
    runtime: &ProxyRuntime,
    official: &mut Option<OfficialSegment>,
    connection_id: &str,
) {
    let Some(segment) = official.as_mut() else {
        return;
    };
    let expired = segment.pending.iter().any(|turn| {
        turn.cancelled_at.is_some_and(|cancelled_at| {
            cancelled_at.elapsed()
                >= std::time::Duration::from_millis(OFFICIAL_CANCEL_DRAIN_TIMEOUT_MS)
        })
    });
    if !expired {
        return;
    }
    segment
        .stages
        .terminal_ms
        .get_or_insert(segment.started.elapsed().as_millis() as u64);
    let stages = segment.stages.clone();
    flush_official_pending(
        runtime,
        &mut segment.pending,
        connection_id,
        "client_disconnect",
        Some("segment_cancel_timeout"),
        Some("Official upstream was silent after client cancellation"),
        &stages,
    );
    close_official_segment(official).await;
}

/// Tear down an Official segment the client connection is moving off of
/// (provider switch), without blocking the dispatch loop on a WebSocket
/// close handshake. A polite `close()` needs its peer to echo the Close
/// frame, which needs someone still polling the read half — nobody is,
/// since the connection is switching away right now — so awaiting it here
/// can hang the whole client connection. Dropping both split halves
/// together (the read half is never separately claimed) closes the
/// underlying TCP connection immediately; the upstream simply sees a
/// dropped connection, which every provider already has to tolerate.
async fn close_official_segment(official: &mut Option<OfficialSegment>) {
    official.take();
}

fn remember_cancelled_response_id(ids: &mut VecDeque<String>, response_id: &str) {
    if ids.iter().any(|cancelled| cancelled == response_id) {
        return;
    }
    if ids.len() >= 64 {
        ids.pop_front();
    }
    ids.push_back(response_id.to_string());
}

fn finish_deferred_official_outcome(
    runtime: &ProxyRuntime,
    turn: &DeferredOfficialTurn,
    connection_id: &str,
    outcome: &str,
    error_category: Option<&str>,
    error: Option<&str>,
) {
    let status = match outcome {
        "success" => 200,
        "client_cancel" | "client_disconnect" => 499,
        _ => 502,
    };
    runtime.finish_terminal_once_with_record(
        &turn.execution_id,
        outcome,
        UsageRecord {
            route_id: turn.plan.route_id.clone(),
            provider: turn.plan.provider_name.clone(),
            model: turn.plan.upstream_model.clone(),
            status,
            error: error.map(str::to_string),
            request_id: Some(turn.request_id.clone()),
            connection_id: Some(connection_id.to_string()),
            outcome: Some(outcome.to_string()),
            error_category: error_category.map(str::to_string),
            control_account_hash: turn.plan.control_account_hash.clone(),
            execution_account_hash: turn.plan.execution_account_hash.clone(),
            selection_revision: turn.plan.selection_revision,
            upstream_attempted: Some(false),
            ..Default::default()
        },
    );
}

fn classify_official_close_reason(reason: &str) -> (&'static str, Option<&'static str>) {
    if reason == "client cancelled" {
        ("client_cancel", Some("client_cancel"))
    } else if reason == "client closed"
        || reason.starts_with("Codex WebSocket read ended")
        || reason == "client send failed"
    {
        ("client_disconnect", Some("client_disconnect"))
    } else if reason.contains("Invalid") || reason.contains("protocol") {
        ("protocol_failure", Some("provider_protocol"))
    } else {
        ("provider_failure", Some("provider_unavailable"))
    }
}

/// Handshake a fresh native Official Responses upstream socket for `plan`,
/// retrying exactly once after a Vellum-managed credential refresh on a
/// 401/403 handshake rejection (Codex-native auth stays Codex-owned; there
/// is nothing Vellum can refresh for it).
#[allow(clippy::too_many_arguments)]
async fn open_official_segment(
    client: &mut WebSocket,
    runtime: &ProxyRuntime,
    inbound_headers: &HeaderMap,
    connection_id: &str,
    request_id: &str,
    execution_id: &str,
    plan: &mut OfficialWebSocketPlan,
) -> Option<OfficialSegment> {
    let started = std::time::Instant::now();
    let mut refreshed = false;
    let handshake_stages = OfficialStageTimes {
        client_accept_ms: 0,
        ..OfficialStageTimes::default()
    };
    tracing::debug!(
        upstream_url = %plan.upstream_url,
        inbound_headers = ?inbound_headers.keys().map(|name| name.as_str()).collect::<Vec<_>>(),
        "opening native Official Responses WebSocket"
    );
    let upstream = loop {
        let handshake_request = match official_websocket_request(plan, inbound_headers) {
            Ok(request) => request,
            Err(error) => {
                record_official_handshake_failure(
                    runtime,
                    plan,
                    request_id,
                    execution_id,
                    connection_id,
                    started,
                    &error,
                    &handshake_stages,
                );
                let _ = send_runtime_failure(client, &error).await;
                return None;
            }
        };
        match run_bounded(
            connect_official_upstream(handshake_request),
            std::time::Duration::from_secs(15),
        )
        .await
        {
            Bounded::Ready(Ok(socket)) => break socket,
            Bounded::Ready(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                if !refreshed && matches!(response.status().as_u16(), 401 | 403) =>
            {
                match runtime.refresh_official_websocket_auth(plan).await {
                    Ok(true) => refreshed = true,
                    Ok(false) => {
                        let error = websocket_handshake_error(response);
                        record_official_handshake_failure(
                            runtime,
                            plan,
                            request_id,
                            execution_id,
                            connection_id,
                            started,
                            &error,
                            &handshake_stages,
                        );
                        let _ = send_runtime_failure(client, &error).await;
                        return None;
                    }
                    Err(error) => {
                        record_official_handshake_failure(
                            runtime,
                            plan,
                            request_id,
                            execution_id,
                            connection_id,
                            started,
                            &error,
                            &handshake_stages,
                        );
                        let _ = send_runtime_failure(client, &error).await;
                        return None;
                    }
                }
            }
            Bounded::Ready(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                let error = websocket_handshake_error(response);
                tracing::warn!(%error, "Official WebSocket handshake rejected");
                record_official_handshake_failure(
                    runtime,
                    plan,
                    request_id,
                    execution_id,
                    connection_id,
                    started,
                    &error,
                    &handshake_stages,
                );
                let _ = send_runtime_failure(client, &error).await;
                return None;
            }
            Bounded::Ready(Err(error)) => {
                let error = RuntimeError::ProviderUnavailable(format!(
                    "Official WebSocket connection failed: {error}"
                ));
                record_official_handshake_failure(
                    runtime,
                    plan,
                    request_id,
                    execution_id,
                    connection_id,
                    started,
                    &error,
                    &handshake_stages,
                );
                let _ = send_runtime_failure(client, &error).await;
                return None;
            }
            Bounded::JoinFailed => {
                let error = RuntimeError::ProviderUnavailable(
                    "Official WebSocket connect task failed".into(),
                );
                record_official_handshake_failure(
                    runtime,
                    plan,
                    request_id,
                    execution_id,
                    connection_id,
                    started,
                    &error,
                    &handshake_stages,
                );
                let _ = send_runtime_failure(client, &error).await;
                return None;
            }
            Bounded::TimedOut => {
                let error = RuntimeError::ProviderUnavailable(
                    "Official WebSocket connection timed out".into(),
                );
                record_official_handshake_failure(
                    runtime,
                    plan,
                    request_id,
                    execution_id,
                    connection_id,
                    started,
                    &error,
                    &handshake_stages,
                );
                let _ = send_runtime_failure(client, &error).await;
                return None;
            }
        }
    };
    let (write, read) = upstream.split();
    Some(OfficialSegment {
        connection_key: plan.connection_key.clone(),
        write,
        read,
        pending: VecDeque::new(),
        stages: OfficialStageTimes {
            client_accept_ms: 0,
            upstream_connected_ms: Some(started.elapsed().as_millis() as u64),
            ..OfficialStageTimes::default()
        },
        started,
    })
}

/// Outcome of attempting to forward one turn's `response.create` upstream.
enum SendOfficialTurnOutcome {
    /// Sent and pushed onto `segment.pending`.
    Sent,
    /// A parent-tree cancel had already landed by the time this turn
    /// registered, before anything reached upstream. The turn's own
    /// terminal row and client notification were already handled; the
    /// segment itself stays open for the next turn.
    AlreadyCancelled,
    /// The upstream write failed; the caller tears the connection down.
    UpstreamSendFailed,
}

/// Send one `response.create` turn on an already-open (fresh or reused)
/// Official segment, rewriting only the model to this turn's own
/// `upstream_model`.
#[allow(clippy::too_many_arguments)]
async fn send_official_turn(
    client: &mut WebSocket,
    runtime: &ProxyRuntime,
    segment: &mut OfficialSegment,
    plan: OfficialWebSocketPlan,
    request: Value,
    request_id: String,
    execution_id: String,
    conversation_key: String,
    connection_id: &str,
) -> SendOfficialTurnOutcome {
    // Register in the shared cancel registry *before* anything reaches
    // upstream, and check it right away: a parent-tree cancel that lands
    // while this turn is still queued behind a (possibly slow) handshake or
    // an earlier in-flight turn must be observed here, not only after the
    // `response.create` frame has already left for the provider (P0:
    // registration used to happen strictly after the upstream send, so an
    // already-cancelled turn was dispatched anyway).
    let (cancel_guard, cancel_rx) = runtime.register_request_cancel(&execution_id);
    if *cancel_rx.borrow() {
        runtime.finish_terminal_once(
            &execution_id,
            &plan.route_id,
            &plan.provider_name,
            &plan.upstream_model,
            &request_id,
            Some(connection_id),
            0,
            None,
            None,
            None,
            "client_cancel",
            Some("client_cancel"),
            Some("cancelled by parent before dispatch"),
            None,
        );
        drop(cancel_guard);
        let _ = send_websocket_failure(client, "request cancelled").await;
        return SendOfficialTurnOutcome::AlreadyCancelled;
    }
    let forwarded = match official_turn_frame(&request, &plan.upstream_model) {
        Ok(message) => message,
        Err(error) => {
            drop(cancel_guard);
            let _ = send_runtime_failure(client, &error).await;
            return SendOfficialTurnOutcome::UpstreamSendFailed;
        }
    };
    let send_started = std::time::Instant::now();
    if send_official_upstream(&mut segment.write, forwarded)
        .await
        .is_err()
    {
        drop(cancel_guard);
        return SendOfficialTurnOutcome::UpstreamSendFailed;
    }
    let forwarded_ms = send_started.elapsed().as_millis() as u64;
    segment
        .stages
        .request_forwarded_ms
        .get_or_insert(segment.started.elapsed().as_millis() as u64);
    let mut turn = OfficialPendingTurn::new(
        plan,
        request_for_history(request),
        request_id,
        execution_id,
        conversation_key,
        cancel_guard,
        cancel_rx,
        send_started,
    );
    turn.request_forwarded_ms = Some(forwarded_ms);
    segment.pending.push_back(turn);
    SendOfficialTurnOutcome::Sent
}

/// Rewrite `response.create.model` to `upstream_model` and wrap as a
/// WebSocket text frame Official can read.
fn official_turn_frame(
    request: &Value,
    upstream_model: &str,
) -> Result<TungsteniteMessage, RuntimeError> {
    prepare_official_frame(
        AxumWsMessage::Text(request.to_string().into()),
        upstream_model,
    )
}

/// A control/cancel frame forwarded to Official verbatim (no field rewrite).
fn text_frame(value: &Value) -> TungsteniteMessage {
    TungsteniteMessage::Text(value.to_string().into())
}

/// Handle one frame read from the currently open Official upstream socket.
/// `false` means the segment (and therefore the whole client connection) is
/// done: either the upstream closed or the downstream client send failed.
async fn handle_official_upstream_frame(
    client: &mut WebSocket,
    runtime: &ProxyRuntime,
    official: &mut Option<OfficialSegment>,
    connection_id: &str,
    upstream_message: Option<Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>,
    frames_out: &mut u64,
) -> bool {
    match upstream_message {
        Some(Ok(TungsteniteMessage::Frame(_))) => {
            tokio::task::yield_now().await;
            true
        }
        Some(Ok(message)) => {
            let Some(segment) = official.as_mut() else {
                return true;
            };
            // A parent-tree cancel is now caught proactively by
            // `next_official_event` racing each pending turn's cancel watch
            // before a frame is ever handed to this function, so no
            // redundant opportunistic check is needed here. A frame that was
            // already buffered when the cancel landed may still be
            // delivered once (accepted: `next_official_event` catches the
            // cancellation on the very next call).
            let application = official_application_frame(&message);
            if application {
                segment
                    .stages
                    .first_upstream_application_frame_ms
                    .get_or_insert(segment.started.elapsed().as_millis() as u64);
                if let Some(turn) = segment.pending.front_mut() {
                    turn.first_byte_ms
                        .get_or_insert(turn.started.elapsed().as_millis() as u64);
                }
            }
            let completed = if application {
                completed_websocket_response(&message)
            } else {
                None
            };
            let failed = if application && completed.is_none() {
                failed_websocket_response(&message)
            } else {
                None
            };
            let closing = matches!(message, TungsteniteMessage::Close(_));
            if client.send(from_tungstenite(message)).await.is_err() || closing {
                return false;
            }
            if application {
                *frames_out += 1;
                let downstream_at = segment.started.elapsed().as_millis() as u64;
                segment
                    .stages
                    .first_downstream_frame_ms
                    .get_or_insert(downstream_at);
                if let Some(turn) = segment.pending.front_mut() {
                    turn.first_downstream_ms
                        .get_or_insert(turn.started.elapsed().as_millis() as u64);
                }
            }
            if let Some(response) = completed {
                segment.stages.terminal_ms = Some(segment.started.elapsed().as_millis() as u64);
                if let Some(mut turn) = segment.pending.pop_front() {
                    if !turn.recorded {
                        turn.terminal_ms
                            .get_or_insert(turn.started.elapsed().as_millis() as u64);
                        let stage_value = segment.stages.for_turn(&turn);
                        if let Some(plan) = turn.plan.as_ref() {
                            if let Err(error) = runtime.record_official_websocket_completion(
                                plan,
                                &turn.request,
                                &turn.conversation_key,
                                &turn.request_id,
                                &turn.execution_id,
                                &response,
                                turn.started.elapsed().as_millis() as u64,
                                turn.first_byte_ms,
                                turn.first_downstream_ms,
                                connection_id,
                                Some(stage_value),
                            ) {
                                tracing::warn!(%error, "failed to persist Official WebSocket completion");
                            } else {
                                turn.recorded = true;
                            }
                        }
                    }
                }
            } else if let Some(failed) = failed {
                segment.stages.terminal_ms = Some(segment.started.elapsed().as_millis() as u64);
                let category = official_websocket_failure_category(&failed);
                let message = failed
                    .pointer("/response/error/message")
                    .or_else(|| failed.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream Official WebSocket failure");
                let outcome = if category == "provider_protocol" || category == "invalid_request" {
                    "protocol_failure"
                } else {
                    "provider_failure"
                };
                if let Some(turn) = segment.pending.front_mut() {
                    record_official_pending_outcome(
                        runtime,
                        turn,
                        connection_id,
                        outcome,
                        Some(category),
                        Some(message),
                        &segment.stages,
                    );
                }
                segment.pending.pop_front();
            }
            true
        }
        Some(Err(error)) => {
            let error = RuntimeError::ProviderUnavailable(format!(
                "Official WebSocket stream failed: {error}"
            ));
            if let Some(segment) = official.as_mut() {
                segment.stages.terminal_ms = Some(segment.started.elapsed().as_millis() as u64);
                flush_official_pending(
                    runtime,
                    &mut segment.pending,
                    connection_id,
                    "provider_failure",
                    Some(error.category()),
                    Some(&error.to_string()),
                    &segment.stages,
                );
            }
            let _ = send_runtime_failure(client, &error).await;
            false
        }
        None => false,
    }
}

/// Run one portable (third-party, or Official mid-materialization) turn to
/// completion through the shared HTTP execute() path, forwarding it back as
/// WebSocket events. Concurrently keeps reading the client so a
/// `response.cancel` ends the turn (and, matching the pre-existing HTTP
/// bridge contract, the whole connection) and any other frame received
/// mid-turn is queued for the next iteration rather than dropped.
// Matches the argument count already established by this file's other
// per-turn accounting helpers (e.g. `record_http_bridge_outcome`).
#[allow(clippy::too_many_arguments)]
async fn execute_portable_turn<S: ProxyRuntimeState>(
    client: &mut WebSocket,
    state: &Arc<S>,
    runtime: &ProxyRuntime,
    headers: &HeaderMap,
    mut request: Value,
    connection_id: &str,
    queued: &mut VecDeque<Value>,
    frames_out: &mut u64,
    force_official_handoff: bool,
) -> TurnOutcome {
    // The HTTP Responses body has no WebSocket event discriminator.
    if request.get("type").and_then(Value::as_str) == Some("response.create") {
        request
            .as_object_mut()
            .expect("response.create is a JSON object")
            .remove("type");
    }
    let mut runtime_request = runtime_request_from_body(state, headers, &request, connection_id);
    runtime_request.metadata.force_portable_official_handoff = force_official_handoff;
    let turn_request_id = runtime_request.metadata.request_id.clone();
    // Minted before dispatch, not read back from `execute` -- this is what
    // lets the `response.cancel` arm below call `cancel_request_tree` with
    // the right key while `execute` is still racing in the `select!` below,
    // not only after it resolves.
    let execution_id = crate::exec::new_execution_id();
    let turn_started = std::time::Instant::now();
    let execute = runtime.execute(runtime_request, &execution_id);
    tokio::pin!(execute);
    let result = loop {
        tokio::select! {
            result = &mut execute => break Some(result),
            inbound = client.recv() => {
                match inbound {
                    None | Some(Err(_)) | Some(Ok(AxumWsMessage::Close(_))) => break None,
                    Some(Ok(AxumWsMessage::Ping(payload))) => {
                        let _ = client.send(AxumWsMessage::Pong(payload)).await;
                    }
                    Some(Ok(AxumWsMessage::Pong(_))) => {}
                    Some(Ok(message)) => {
                        match websocket_request_value(&message) {
                            Ok(value)
                                if value.get("type").and_then(Value::as_str)
                                    == Some("response.cancel") =>
                            {
                                if send_websocket_failure(client, "request cancelled").await {
                                    *frames_out += 1;
                                }
                                runtime.cancel_request_tree(&execution_id);
                                runtime.unregister_request_cancel(&execution_id);
                                record_http_bridge_outcome(
                                    runtime,
                                    &request,
                                    &execution_id,
                                    &turn_request_id,
                                    connection_id,
                                    turn_started,
                                    "client_cancel",
                                    Some("client_cancel"),
                                    Some("request cancelled"),
                                    None,
                                    None,
                                );
                                return TurnOutcome::Stop("client cancelled".into());
                            }
                            Ok(value) => queued.push_back(value),
                            Err(_) => {}
                        }
                    }
                }
            }
        }
    };
    let Some(result) = result else {
        record_http_bridge_outcome(
            runtime,
            &request,
            &execution_id,
            &turn_request_id,
            connection_id,
            turn_started,
            "client_disconnect",
            Some("client_disconnect"),
            Some("client closed"),
            None,
            None,
        );
        return TurnOutcome::Stop("client closed".into());
    };
    match result {
        Ok(RuntimeResponse::Json(value)) => {
            match send_buffered_response_as_websocket_events(client, value, turn_started).await {
                Ok(sent) => {
                    *frames_out += sent.frames_out;
                    if let Some(ms) = sent.first_downstream_ms {
                        runtime.mark_first_downstream_frame(&turn_request_id, ms);
                    }
                    TurnOutcome::Continue
                }
                Err(()) => TurnOutcome::Stop("client send failed".into()),
            }
        }
        Ok(RuntimeResponse::Sse(stream)) => {
            match send_stream_as_websocket_events(client, stream, turn_started).await {
                Ok(outcome) => {
                    *frames_out += outcome.frames_out;
                    for value in outcome.queued {
                        queued.push_back(value);
                    }
                    if let Some(ms) = outcome.first_downstream_ms {
                        runtime.mark_first_downstream_frame(&turn_request_id, ms);
                    }
                    match outcome.stop {
                        Some(stop) => TurnOutcome::Stop(stop),
                        None => {
                            // The stream ended without a terminal event. A
                            // parent-cancel fan-out drops a child's upstream
                            // stream this way; give that child its own bounded
                            // 499 row (one terminal row per request).
                            if runtime.is_request_cancelled(&execution_id) {
                                record_http_bridge_outcome(
                                    runtime,
                                    &request,
                                    &execution_id,
                                    &turn_request_id,
                                    connection_id,
                                    turn_started,
                                    "client_cancel",
                                    Some("client_cancel"),
                                    Some("cancelled by parent"),
                                    None,
                                    None,
                                );
                            }
                            TurnOutcome::Continue
                        }
                    }
                }
                Err(stop) => {
                    let outcome = if stop == "client cancelled" {
                        "client_cancel"
                    } else {
                        "client_disconnect"
                    };
                    record_http_bridge_outcome(
                        runtime,
                        &request,
                        &execution_id,
                        &turn_request_id,
                        connection_id,
                        turn_started,
                        outcome,
                        Some(outcome),
                        Some(&stop),
                        None,
                        None,
                    );
                    if outcome == "client_cancel" {
                        runtime.cancel_request_tree(&execution_id);
                    }
                    runtime.unregister_request_cancel(&execution_id);
                    TurnOutcome::Stop(stop)
                }
            }
        }
        Ok(RuntimeResponse::Raw { status, body, .. }) => {
            if !(200..300).contains(&status) {
                if send_websocket_failure(client, &format!("upstream HTTP {status}")).await {
                    *frames_out += 1;
                }
                return TurnOutcome::Stop(format!("upstream HTTP {status}"));
            }
            match serde_json::from_slice(body.as_slice()) {
                Ok(value) => {
                    match send_buffered_response_as_websocket_events(client, value, turn_started)
                        .await
                    {
                        Ok(sent) => {
                            *frames_out += sent.frames_out;
                            if let Some(ms) = sent.first_downstream_ms {
                                runtime.mark_first_downstream_frame(&turn_request_id, ms);
                            }
                            TurnOutcome::Continue
                        }
                        Err(()) => TurnOutcome::Stop("client send failed".into()),
                    }
                }
                Err(error) => {
                    if send_websocket_failure(client, &format!("invalid upstream JSON: {error}"))
                        .await
                    {
                        *frames_out += 1;
                    }
                    TurnOutcome::Stop("invalid upstream JSON".into())
                }
            }
        }
        Err(error) => {
            if send_runtime_failure(client, &error).await {
                *frames_out += 1;
            }
            TurnOutcome::Stop(error.to_string())
        }
    }
}

fn request_for_history(mut value: Value) -> Value {
    if value.get("type").and_then(Value::as_str) == Some("response.create") {
        if let Some(object) = value.as_object_mut() {
            object.remove("type");
        }
    }
    value
}

fn completed_websocket_response(message: &TungsteniteMessage) -> Option<Value> {
    websocket_event(message).and_then(|event| {
        (event.get("type").and_then(Value::as_str) == Some("response.completed"))
            .then(|| event.get("response").cloned())
            .flatten()
    })
}

fn failed_websocket_response(message: &TungsteniteMessage) -> Option<Value> {
    websocket_event(message).and_then(|event| {
        matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.failed" | "error")
        )
        .then_some(event)
    })
}

fn official_websocket_failure_category(event: &Value) -> &str {
    if let Some(category) = event
        .pointer("/response/error/category")
        .and_then(Value::as_str)
    {
        return category;
    }
    match event.pointer("/error/type").and_then(Value::as_str) {
        Some("invalid_request_error") => "invalid_request",
        Some("authentication_error") => "provider_auth",
        Some("insufficient_quota") | Some("rate_limit_error") => "provider_quota",
        Some(_) | None => "provider_protocol",
    }
}

fn websocket_event(message: &TungsteniteMessage) -> Option<Value> {
    let bytes = match message {
        TungsteniteMessage::Text(text) => text.as_bytes(),
        TungsteniteMessage::Binary(bytes) => bytes.as_ref(),
        _ => return None,
    };
    serde_json::from_slice(bytes).ok()
}

fn official_failure_outcome(error: &RuntimeError) -> &'static str {
    match error.category() {
        "provider_protocol" | "invalid_request" => "protocol_failure",
        "client_cancel" => "client_cancel",
        _ => "provider_failure",
    }
}

#[allow(clippy::too_many_arguments)]
fn record_official_handshake_failure(
    runtime: &ProxyRuntime,
    plan: &OfficialWebSocketPlan,
    request_id: &str,
    execution_id: &str,
    connection_id: &str,
    started: std::time::Instant,
    error: &RuntimeError,
    stages: &OfficialStageTimes,
) {
    runtime.finish_terminal_once(
        execution_id,
        &plan.route_id,
        &plan.provider_name,
        &plan.upstream_model,
        request_id,
        Some(connection_id),
        started.elapsed().as_millis() as u64,
        None,
        None,
        None,
        official_failure_outcome(error),
        Some(error.category()),
        Some(&error.to_string()),
        Some(stages.to_value()),
    );
}

/// Outcome of [`run_bounded`]: a spawned future either finishes, panics, or
/// is aborted for running past its deadline.
enum Bounded<T> {
    Ready(T),
    JoinFailed,
    TimedOut,
}

/// Run `future` on its own task with a deadline. A bare `tokio::time::timeout`
/// around a `JoinHandle` drops the handle on expiry without stopping the
/// task it owns, leaving a detached upstream connect running unobserved;
/// aborting the handle explicitly is what actually stops it.
async fn run_bounded<F>(future: F, timeout: std::time::Duration) -> Bounded<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut handle = tokio::spawn(future);
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(value)) => Bounded::Ready(value),
        Ok(Err(_)) => Bounded::JoinFailed,
        Err(_) => {
            handle.abort();
            Bounded::TimedOut
        }
    }
}

#[cfg(test)]
fn official_upstream_handshake_request(
    plan: &OfficialWebSocketPlan,
    inbound_headers: &HeaderMap,
) -> Result<
    tokio_tungstenite::tungstenite::http::Request<()>,
    Box<tokio_tungstenite::tungstenite::Error>,
> {
    official_websocket_request(plan, inbound_headers).map_err(|error| {
        Box::new(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string()),
        ))
    })
}

/// Official upstream connect with Nagle disabled so the first small
/// `response.create` is not held in the TCP stack after TLS.
async fn connect_official_upstream(
    request: tokio_tungstenite::tungstenite::http::Request<()>,
) -> Result<OfficialUpstreamSocket, tokio_tungstenite::tungstenite::Error> {
    connect_async_with_config(request, None, true)
        .await
        .map(|(socket, _)| socket)
}

/// Write one Official frame and flush rustls/TCP so the first create is
/// visible upstream without a second client write.
async fn send_official_upstream<S>(sink: &mut S, message: TungsteniteMessage) -> Result<(), ()>
where
    S: futures_util::Sink<TungsteniteMessage> + Unpin,
{
    sink.send(message).await.map_err(|_| ())?;
    sink.flush().await.map_err(|_| ())
}

fn official_websocket_request(
    plan: &OfficialWebSocketPlan,
    inbound_headers: &HeaderMap,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, RuntimeError> {
    official_websocket_request_parts(
        &plan.upstream_url,
        inbound_headers,
        &plan.auth_headers,
        plan.preserve_incoming_auth,
    )
}

fn official_websocket_request_parts(
    upstream_url: &str,
    inbound_headers: &HeaderMap,
    auth_headers: &[(String, String)],
    preserve_incoming_auth: bool,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, RuntimeError> {
    // `IntoClientRequest` supplies a complete RFC 6455 handshake. Constructing
    // a bare HTTP request here would omit the generated key/version headers.
    let mut request = upstream_url.into_client_request().map_err(|error| {
        RuntimeError::Internal(format!("build Official WebSocket handshake: {error}"))
    })?;
    for (name, value) in inbound_headers {
        if should_forward_official_header(name.as_str())
            || (preserve_incoming_auth
                && matches!(
                    name.as_str(),
                    "authorization" | "openai-account" | "chatgpt-account-id"
                ))
        {
            request.headers_mut().insert(name.clone(), value.clone());
        }
    }
    for (name, value) in auth_headers {
        let name = tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| {
                RuntimeError::Internal(format!("invalid auth header name: {error}"))
            })?;
        let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value).map_err(
            |error| RuntimeError::Internal(format!("invalid auth header value: {error}")),
        )?;
        request.headers_mut().insert(name, value);
    }
    Ok(request)
}

/// Preserve end-to-end Codex/OpenAI metadata, including future `x-codex-*`
/// fields, while refusing loopback, proxy and RFC hop-by-hop headers.
fn should_forward_official_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "user-agent"
        || name == "openai-beta"
        || name == "traceparent"
        || name == "tracestate"
        || name == "baggage"
        || name == "x-request-id"
        || name.starts_with("x-codex-")
        || name.starts_with("x-openai-")
}

/// Only `response.create.model` is rewritten, because catalog IDs are local
/// aliases. Text/binary shape and every other application/control frame are
/// forwarded unchanged.
fn prepare_official_frame(
    message: AxumWsMessage,
    upstream_model: &str,
) -> Result<TungsteniteMessage, RuntimeError> {
    match message {
        AxumWsMessage::Text(text) => {
            let Some(prepared) = prepare_response_create_bytes(text.as_bytes(), upstream_model)?
            else {
                return Ok(TungsteniteMessage::Text(text.to_string().into()));
            };
            let prepared =
                String::from_utf8(prepared).expect("serde_json always emits valid UTF-8");
            Ok(TungsteniteMessage::Text(prepared.into()))
        }
        AxumWsMessage::Binary(bytes) => {
            let Some(prepared) = prepare_response_create_bytes(bytes.as_ref(), upstream_model)?
            else {
                return Ok(TungsteniteMessage::Binary(bytes));
            };
            Ok(TungsteniteMessage::Binary(prepared.into()))
        }
        AxumWsMessage::Ping(bytes) => Ok(TungsteniteMessage::Ping(bytes)),
        AxumWsMessage::Pong(bytes) => Ok(TungsteniteMessage::Pong(bytes)),
        AxumWsMessage::Close(_) => Ok(TungsteniteMessage::Close(None)),
    }
}

fn prepare_response_create_bytes(
    bytes: &[u8],
    upstream_model: &str,
) -> Result<Option<Vec<u8>>, RuntimeError> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|error| {
        RuntimeError::InvalidRequest(format!("Invalid Codex WebSocket JSON request: {error}"))
    })?;
    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return Ok(None);
    }
    let object = value.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidRequest("response.create must be a JSON object".into())
    })?;
    object.insert("model".into(), Value::String(upstream_model.to_string()));
    serde_json::to_vec(&value)
        .map(Some)
        .map_err(|error| RuntimeError::Internal(format!("encode response.create: {error}")))
}

fn from_tungstenite(message: TungsteniteMessage) -> AxumWsMessage {
    match message {
        TungsteniteMessage::Text(value) => AxumWsMessage::Text(value.to_string().into()),
        TungsteniteMessage::Binary(value) => AxumWsMessage::Binary(value),
        TungsteniteMessage::Ping(value) => AxumWsMessage::Ping(value),
        TungsteniteMessage::Pong(value) => AxumWsMessage::Pong(value),
        TungsteniteMessage::Close(_) => AxumWsMessage::Close(None),
        TungsteniteMessage::Frame(_) => unreachable!("raw frames are filtered before conversion"),
    }
}

fn websocket_handshake_error(
    response: tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) -> RuntimeError {
    let status = response.status().as_u16();
    let diagnostic = response
        .body()
        .as_deref()
        .map(|body| {
            String::from_utf8_lossy(body)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(512)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "no provider diagnostic".into());
    let message = format!("Official WebSocket handshake HTTP {status}: {diagnostic}");
    match status {
        401 | 403 => RuntimeError::ProviderUnauthorized(message),
        429 => RuntimeError::provider_quota(message, None, true),
        400 | 408 | 409 | 425 | 426 => RuntimeError::ProviderProtocol(message),
        500..=599 => RuntimeError::ProviderUnavailable(message),
        _ => RuntimeError::ProviderProtocol(message),
    }
}

/// Build the same [`RuntimeRequest`] the HTTP boundary would, from the
/// WebSocket JSON body and the upgrade headers.
fn runtime_request_from_body<S: ProxyRuntimeState>(
    state: &Arc<S>,
    headers: &HeaderMap,
    body: &Value,
    connection_id: &str,
) -> RuntimeRequest {
    let incoming_auth = IncomingAuthContext {
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        openai_account: headers
            .get("openai-account")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    };
    let metadata = crate::server::build_request_metadata(
        headers,
        body,
        RuntimeEndpoint::Responses,
        Some(connection_id.to_string()),
    );
    RuntimeRequest {
        body: body.clone(),
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth,
        execution_environment: state.execution_environment(),
        metadata,
    }
}

fn websocket_upstream_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

fn websocket_request_value(message: &AxumWsMessage) -> Result<Value, String> {
    let bytes = match message {
        AxumWsMessage::Text(text) => text.as_bytes(),
        AxumWsMessage::Binary(bytes) => bytes.as_ref(),
        _ => {
            return Err("Vellum expected a JSON text or binary WebSocket request from Codex".into())
        }
    };
    serde_json::from_slice(bytes)
        .map_err(|error| format!("Invalid Codex WebSocket JSON request: {error}"))
}

#[derive(Default)]
struct StreamForwardOutcome {
    frames_out: u64,
    queued: Vec<Value>,
    stop: Option<String>,
    first_downstream_ms: Option<u64>,
}

#[derive(Default, Clone)]
struct OfficialStageTimes {
    client_accept_ms: u64,
    upstream_connected_ms: Option<u64>,
    request_forwarded_ms: Option<u64>,
    first_upstream_application_frame_ms: Option<u64>,
    first_downstream_frame_ms: Option<u64>,
    terminal_ms: Option<u64>,
}

impl OfficialStageTimes {
    fn to_value(&self) -> Value {
        json!({
            "clientAcceptMs": self.client_accept_ms,
            "upstreamConnectedMs": self.upstream_connected_ms,
            "requestForwardedMs": self.request_forwarded_ms,
            "firstUpstreamApplicationFrameMs": self.first_upstream_application_frame_ms,
            "firstDownstreamFrameMs": self.first_downstream_frame_ms,
            "terminalMs": self.terminal_ms,
        })
    }

    fn for_turn(&self, turn: &OfficialPendingTurn) -> Value {
        json!({
            "clientAcceptMs": self.client_accept_ms,
            "upstreamConnectedMs": self.upstream_connected_ms,
            "requestForwardedMs": turn.request_forwarded_ms,
            "firstUpstreamApplicationFrameMs": turn.first_byte_ms,
            "firstDownstreamFrameMs": turn.first_downstream_ms,
            "terminalMs": turn.terminal_ms.or(self.terminal_ms),
        })
    }
}

/// One Official turn forwarded on a segment but not yet terminal. Carries
/// its own [`OfficialWebSocketPlan`] (route/model identity plus the
/// admission guard and any managed-auth token) so a segment reused across
/// several turns still attributes each completion to the model it was
/// actually sent with — never the socket's first turn. `plan` is `None`
/// only for unit tests that exercise stage-time bookkeeping in isolation.
struct OfficialPendingTurn {
    plan: Option<OfficialWebSocketPlan>,
    request: Value,
    request_id: String,
    /// This turn's internal cancel-registry key (see
    /// [`crate::exec::new_execution_id`]). Distinct from `request_id`
    /// (external correlation) so a WebSocket client repeating the same
    /// `x-request-id` across turns can never collide two turns' cancel
    /// sockets or terminal-dedupe entries.
    execution_id: String,
    /// Durable history/canonical owner resolved from this exact frame's
    /// normalized Codex metadata before the request is forwarded upstream.
    conversation_key: String,
    /// Releases this turn's cancel socket on drop (segment torn down,
    /// connection closed) if [`record_official_pending_outcome`] never got a
    /// chance to release it explicitly. `None` only for unit tests that
    /// exercise stage-time bookkeeping in isolation.
    _cancel_guard: Option<crate::exec::CancelRegistration>,
    /// Watched by [`next_official_event`] so a parent-tree cancel landing on
    /// a different connection is observed even when the provider never
    /// sends another frame. `None` only for unit tests that exercise
    /// stage-time bookkeeping in isolation.
    cancel_rx: Option<watch::Receiver<bool>>,
    started: std::time::Instant,
    request_forwarded_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    first_downstream_ms: Option<u64>,
    terminal_ms: Option<u64>,
    recorded: bool,
    cancelled_at: Option<std::time::Instant>,
}

impl OfficialPendingTurn {
    #[allow(clippy::too_many_arguments)]
    fn new(
        plan: OfficialWebSocketPlan,
        request: Value,
        request_id: String,
        execution_id: String,
        conversation_key: String,
        cancel_guard: crate::exec::CancelRegistration,
        cancel_rx: watch::Receiver<bool>,
        started: std::time::Instant,
    ) -> Self {
        Self {
            plan: Some(plan),
            request,
            request_id,
            execution_id,
            conversation_key,
            _cancel_guard: Some(cancel_guard),
            cancel_rx: Some(cancel_rx),
            started,
            request_forwarded_ms: None,
            first_byte_ms: None,
            first_downstream_ms: None,
            terminal_ms: None,
            recorded: false,
            cancelled_at: None,
        }
    }
}

fn record_official_pending_outcome(
    runtime: &ProxyRuntime,
    turn: &mut OfficialPendingTurn,
    connection_id: &str,
    outcome: &str,
    error_category: Option<&str>,
    error: Option<&str>,
    stages: &OfficialStageTimes,
) {
    if turn.recorded {
        return;
    }
    turn.recorded = true;
    turn.terminal_ms
        .get_or_insert(turn.started.elapsed().as_millis() as u64);
    let Some(plan) = turn.plan.as_ref() else {
        return;
    };
    runtime.finish_terminal_once(
        &turn.execution_id,
        &plan.route_id,
        &plan.provider_name,
        &plan.upstream_model,
        &turn.request_id,
        Some(connection_id),
        turn.started.elapsed().as_millis() as u64,
        turn.first_byte_ms,
        turn.first_byte_ms,
        turn.first_downstream_ms,
        outcome,
        error_category,
        error,
        Some(stages.for_turn(turn)),
    );
}

fn flush_official_pending(
    runtime: &ProxyRuntime,
    pending: &mut VecDeque<OfficialPendingTurn>,
    connection_id: &str,
    outcome: &str,
    error_category: Option<&str>,
    error: Option<&str>,
    stages: &OfficialStageTimes,
) {
    for turn in pending.iter_mut() {
        record_official_pending_outcome(
            runtime,
            turn,
            connection_id,
            outcome,
            error_category,
            error,
            stages,
        );
    }
    pending.clear();
}

fn official_application_frame(message: &TungsteniteMessage) -> bool {
    matches!(
        message,
        TungsteniteMessage::Text(_) | TungsteniteMessage::Binary(_)
    )
}

#[allow(clippy::too_many_arguments)]
fn record_http_bridge_outcome(
    runtime: &ProxyRuntime,
    request: &Value,
    execution_id: &str,
    request_id: &str,
    connection_id: &str,
    started: std::time::Instant,
    outcome: &str,
    error_category: Option<&str>,
    error: Option<&str>,
    first_byte_ms: Option<u64>,
    first_downstream_ms: Option<u64>,
) {
    let catalog_id = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let (route_id, provider, model) = runtime.resolved_usage_identity(catalog_id);
    let duration_ms = started.elapsed().as_millis() as u64;
    let stages = json!({
        "clientAcceptMs": 0,
        "requestForwardedMs": 0,
        "firstUpstreamApplicationFrameMs": first_byte_ms,
        "firstDownstreamFrameMs": first_downstream_ms,
        "terminalMs": duration_ms,
    });
    runtime.finish_terminal_once(
        execution_id,
        &route_id,
        &provider,
        &model,
        request_id,
        Some(connection_id),
        duration_ms,
        first_byte_ms,
        first_byte_ms,
        first_downstream_ms,
        outcome,
        error_category,
        error,
        Some(stages),
    );
}

/// Forward a runtime SSE stream as WebSocket text messages. Each block's
/// `data:` JSON is sent as one message; `response.completed`/`response.failed`
/// mark the terminal event. Returns the number of application frames sent.
async fn send_stream_as_websocket_events(
    client: &mut WebSocket,
    stream: futures_util::stream::BoxStream<
        'static,
        Result<axum::body::Bytes, crate::error::RuntimeError>,
    >,
    turn_started: std::time::Instant,
) -> Result<StreamForwardOutcome, String> {
    let mut source = stream;
    let mut body = String::new();
    let mut remainder = Vec::new();
    let mut terminal_seen = false;
    let mut outcome = StreamForwardOutcome::default();
    loop {
        let item = tokio::select! {
            item = source.next() => item,
            inbound = client.recv() => {
                match inbound {
                    None | Some(Err(_)) | Some(Ok(AxumWsMessage::Close(_))) => {
                        return Err("client closed".into());
                    }
                    Some(Ok(AxumWsMessage::Ping(payload))) => {
                        let _ = client.send(AxumWsMessage::Pong(payload)).await;
                        continue;
                    }
                    Some(Ok(AxumWsMessage::Pong(_))) => continue,
                    Some(Ok(message)) => {
                        match websocket_request_value(&message) {
                            Ok(value)
                                if value.get("type").and_then(Value::as_str)
                                    == Some("response.cancel") =>
                            {
                                if send_websocket_failure(client, "request cancelled").await {
                                    outcome.frames_out += 1;
                                    outcome
                                        .first_downstream_ms
                                        .get_or_insert(turn_started.elapsed().as_millis() as u64);
                                }
                                return Err("client cancelled".into());
                            }
                            Ok(value) => outcome.queued.push(value),
                            Err(_) => {}
                        }
                        continue;
                    }
                }
            }
        };
        let Some(item) = item else {
            break;
        };
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(error) => {
                if send_runtime_failure(client, &error).await {
                    outcome.frames_out += 1;
                    outcome
                        .first_downstream_ms
                        .get_or_insert(turn_started.elapsed().as_millis() as u64);
                }
                outcome.stop = Some(error.category().into());
                return Ok(outcome);
            }
        };
        append_utf8_safe(&mut body, &mut remainder, &bytes);
        while let Some(block) = take_sse_block(&mut body) {
            match send_sse_block_as_websocket_event(client, &block, &mut terminal_seen).await {
                Ok(true) => {
                    outcome.frames_out += 1;
                    outcome
                        .first_downstream_ms
                        .get_or_insert(turn_started.elapsed().as_millis() as u64);
                }
                Ok(false) => {}
                Err(()) => return Err("client send failed".into()),
            }
        }
    }
    if !body.trim().is_empty() {
        body.push_str("\n\n");
        while let Some(block) = take_sse_block(&mut body) {
            match send_sse_block_as_websocket_event(client, &block, &mut terminal_seen).await {
                Ok(true) => {
                    outcome.frames_out += 1;
                    outcome
                        .first_downstream_ms
                        .get_or_insert(turn_started.elapsed().as_millis() as u64);
                }
                Ok(false) => {}
                Err(()) => return Err("client send failed".into()),
            }
        }
    }
    if !terminal_seen
        && send_websocket_failure(
            client,
            "Upstream stream ended without response.completed or response.failed",
        )
        .await
    {
        outcome.frames_out += 1;
    }
    Ok(outcome)
}

/// Send one SSE block's `data:` payload as a WebSocket text message,
/// skipping empty blocks and the `[DONE]` marker (Desktop parity).
/// `Ok(true)` sent an application frame, `Ok(false)` skipped a marker, `Err`
/// means the client socket is gone.
async fn send_sse_block_as_websocket_event(
    client: &mut WebSocket,
    block: &str,
    terminal_seen: &mut bool,
) -> Result<bool, ()> {
    let Some(data) = block.lines().find_map(|line| strip_sse_field(line, "data")) else {
        return Ok(false);
    };
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(false);
    }
    if let Ok(value) = serde_json::from_str::<Value>(data) {
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed")
        ) {
            *terminal_seen = true;
        }
    }
    client
        .send(AxumWsMessage::Text(data.to_string().into()))
        .await
        .map(|_| true)
        .map_err(|_| ())
}

/// Replay a buffered JSON response as `response.output_item.*` /
/// `response.output_text.*` / `response.completed` WebSocket messages with
/// monotonically increasing sequence numbers (Desktop parity).
fn buffered_application_frames(response: &Value) -> Vec<Value> {
    let mut sequence_number = 0u64;
    let mut frames = Vec::new();
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for (output_index, item) in output.into_iter().enumerate() {
        frames.push(json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "output_index": output_index,
            "item": item.clone()
        }));
        sequence_number += 1;

        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (content_index, part) in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    frames.push(json!({
                        "type": "response.output_text.delta",
                        "sequence_number": sequence_number,
                        "output_index": output_index,
                        "content_index": content_index,
                        "delta": text
                    }));
                    sequence_number += 1;
                    frames.push(json!({
                        "type": "response.output_text.done",
                        "sequence_number": sequence_number,
                        "output_index": output_index,
                        "content_index": content_index,
                        "text": text
                    }));
                    sequence_number += 1;
                }
            }
        }

        frames.push(json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number,
            "output_index": output_index,
            "item": item
        }));
        sequence_number += 1;
    }

    frames.push(json!({
        "type": "response.completed",
        "sequence_number": sequence_number,
        "response": response
    }));
    frames
}

async fn send_buffered_response_as_websocket_events(
    client: &mut WebSocket,
    response: Value,
    turn_started: std::time::Instant,
) -> Result<StreamForwardOutcome, ()> {
    let frames = buffered_application_frames(&response);
    let mut outcome = StreamForwardOutcome::default();
    for frame in frames {
        if client
            .send(AxumWsMessage::Text(frame.to_string().into()))
            .await
            .is_err()
        {
            return Err(());
        }
        outcome.frames_out += 1;
        outcome
            .first_downstream_ms
            .get_or_insert(turn_started.elapsed().as_millis() as u64);
    }
    Ok(outcome)
}

/// The client-visible failure envelope for WebSocket-level errors (Desktop
/// parity: `vellum_proxy_error`).
pub async fn send_websocket_failure(client: &mut WebSocket, message: &str) -> bool {
    let value = json!({
        "type": "response.failed",
        "sequence_number": 0,
        "response": {
            "object": "response",
            "status": "failed",
            "error": {
                "type": "vellum_proxy_error",
                "message": message
            }
        }
    });
    client
        .send(AxumWsMessage::Text(value.to_string().into()))
        .await
        .is_ok()
}

async fn send_runtime_failure(client: &mut WebSocket, error: &RuntimeError) -> bool {
    let value = json!({
        "type": "response.failed",
        "sequence_number": 0,
        "response": {
            "object": "response",
            "status": "failed",
            "error": {
                "type": "vellum_proxy_error",
                "code": error.http_status().as_u16(),
                "category": error.category(),
                "message": error.to_string()
            }
        }
    });
    client
        .send(AxumWsMessage::Text(value.to_string().into()))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProxyRuntimeConfig, RuntimeRouteConfig};
    use crate::inbound::InboundAccessPolicy;
    use crate::route::{RuntimeAuthKind, RuntimeProviderKind, RuntimeWireFormat};
    use crate::server::build_headless_router;
    use crate::state::StaticProxyState;
    use axum::body::Bytes;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn response_create_discriminator_is_preserved_for_route_selection() {
        let message = AxumWsMessage::Text(
            r#"{"type":"response.create","model":"gpt-5.6-luna","input":[],"stream":true}"#.into(),
        );

        let value = websocket_request_value(&message).unwrap();

        assert_eq!(value["type"], "response.create");
        assert_eq!(value["model"], "gpt-5.6-luna");
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn official_turn_stages_use_per_turn_forwarded_and_terminal() {
        let first = OfficialPendingTurn {
            plan: None,
            request: json!({}),
            request_id: "req_1".into(),
            execution_id: "exec_1".into(),
            conversation_key: "codex:s:t1".into(),
            _cancel_guard: None,
            cancel_rx: None,
            started: std::time::Instant::now(),
            request_forwarded_ms: Some(12),
            first_byte_ms: Some(20),
            first_downstream_ms: Some(22),
            terminal_ms: Some(40),
            recorded: false,
            cancelled_at: None,
        };
        let second = OfficialPendingTurn {
            plan: None,
            request: json!({}),
            request_id: "req_2".into(),
            execution_id: "exec_2".into(),
            conversation_key: "codex:s:t2".into(),
            _cancel_guard: None,
            cancel_rx: None,
            started: std::time::Instant::now(),
            request_forwarded_ms: Some(80),
            first_byte_ms: Some(90),
            first_downstream_ms: Some(95),
            terminal_ms: Some(120),
            recorded: false,
            cancelled_at: None,
        };
        let stages = OfficialStageTimes {
            client_accept_ms: 0,
            upstream_connected_ms: Some(5),
            request_forwarded_ms: Some(12),
            first_upstream_application_frame_ms: Some(20),
            first_downstream_frame_ms: Some(22),
            terminal_ms: Some(120),
        };
        assert_eq!(stages.for_turn(&first)["requestForwardedMs"], 12);
        assert_eq!(stages.for_turn(&first)["terminalMs"], 40);
        assert_eq!(stages.for_turn(&second)["requestForwardedMs"], 80);
        assert_eq!(stages.for_turn(&second)["firstDownstreamFrameMs"], 95);
        assert_eq!(stages.for_turn(&second)["terminalMs"], 120);
    }

    #[test]
    fn official_response_create_changes_only_the_model() {
        let original = br#"{"type":"response.create","model":"catalog-alias","input":[{"type":"reasoning","encrypted_content":"opaque"}],"stream":true}"#;
        let prepared = prepare_response_create_bytes(original, "gpt-upstream")
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&prepared).unwrap();
        assert_eq!(value["type"], "response.create");
        assert_eq!(value["model"], "gpt-upstream");
        assert_eq!(value["input"][0]["encrypted_content"], "opaque");
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn non_create_frames_are_not_reencoded() {
        let original = br#"{"type":"response.cancel","response_id":"resp_1"}"#;
        assert!(prepare_response_create_bytes(original, "gpt-upstream")
            .unwrap()
            .is_none());
    }

    #[test]
    fn official_header_filter_keeps_codex_metadata_and_drops_loopback_state() {
        assert!(should_forward_official_header("x-codex-turn-metadata"));
        assert!(should_forward_official_header("x-codex-installation-id"));
        assert!(should_forward_official_header("openai-beta"));
        assert!(should_forward_official_header("user-agent"));
        assert!(!should_forward_official_header("host"));
        assert!(!should_forward_official_header("sec-websocket-key"));
        assert!(!should_forward_official_header("cookie"));
        assert!(!should_forward_official_header("origin"));
        assert!(!should_forward_official_header("authorization"));
    }

    #[test]
    fn handshake_failure_preserves_category_and_bounds_provider_diagnostic() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(400)
            .body(Some(vec![b'x'; 2_000]))
            .unwrap();
        let error = websocket_handshake_error(response);
        assert_eq!(error.category(), "provider_protocol");
        assert!(error.to_string().contains("HTTP 400"));
        assert!(error.to_string().len() < 620, "{error}");
    }

    #[tokio::test]
    async fn official_websocket_is_one_bidirectional_upstream_connection_for_multiple_turns() {
        #[derive(Clone, Default)]
        struct Capture {
            connections: Arc<AtomicUsize>,
            headers: Arc<Mutex<Option<HeaderMap>>>,
            frames: Arc<Mutex<Vec<Value>>>,
        }

        let capture = Capture::default();
        let upstream_capture = capture.clone();
        let http_hits = Arc::new(AtomicUsize::new(0));
        let http_hits_for_route = Arc::clone(&http_hits);
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
                let capture = upstream_capture.clone();
                async move {
                    capture.connections.fetch_add(1, Ordering::SeqCst);
                    *capture.headers.lock().unwrap() = Some(headers);
                    ws.on_upgrade(move |mut socket| async move {
                        let mut turn = 0usize;
                        while let Some(Ok(message)) = socket.recv().await {
                            let AxumWsMessage::Text(text) = message else {
                                continue;
                            };
                            let value: Value = serde_json::from_str(&text).unwrap();
                            capture.frames.lock().unwrap().push(value.clone());
                            if value.get("type").and_then(Value::as_str) == Some("response.create")
                            {
                                turn += 1;
                                let completed = json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": format!("resp_{turn}"),
                                        "object": "response",
                                        "status": "completed",
                                        "output": []
                                    }
                                });
                                socket
                                    .send(AxumWsMessage::Text(completed.to_string().into()))
                                    .await
                                    .unwrap();
                            }
                        }
                    })
                }
            })
            .post(move || {
                let http_hits_for_route = Arc::clone(&http_hits_for_route);
                async move {
                    http_hits_for_route.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::NOT_IMPLEMENTED
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "official".into(),
            catalog_id: "vellum-official".into(),
            name: "Official".into(),
            base_url: format!("http://{upstream_address}/backend-api/codex"),
            provider_kind: RuntimeProviderKind::Official,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "gpt-upstream".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let mut request = format!("ws://{proxy_address}/v1/responses")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("x-codex-turn-metadata", "native-metadata".parse().unwrap());
        request
            .headers_mut()
            .insert("cookie", "must-not-leak=1".parse().unwrap());
        let (mut client, _) = tokio_tungstenite::connect_async(request).await.unwrap();

        for turn in 1..=2 {
            client
                .send(TungsteniteMessage::Text(
                    json!({
                        "type": "response.create",
                        "model": "vellum-official",
                        "input": [{"role": "user", "content": format!("turn {turn}")}],
                        "stream": true
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let TungsteniteMessage::Text(text) = response else {
                panic!("expected response.completed text frame");
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["response"]["id"], format!("resp_{turn}"));
        }
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "resp_2"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if capture.frames.lock().unwrap().len() == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(capture.connections.load(Ordering::SeqCst), 1);
        assert_eq!(
            http_hits.load(Ordering::SeqCst),
            0,
            "Official WS must not fall back to HTTP /responses"
        );
        {
            let frames = capture.frames.lock().unwrap();
            assert_eq!(frames[0]["type"], "response.create");
            assert_eq!(frames[0]["model"], "gpt-upstream");
            assert_eq!(frames[1]["model"], "gpt-upstream");
            assert_eq!(frames[2]["type"], "response.cancel");
            let headers = capture.headers.lock().unwrap();
            let headers = headers.as_ref().unwrap();
            assert_eq!(headers["x-codex-turn-metadata"], "native-metadata");
            assert!(!headers.contains_key("cookie"));
        }
        let usage = runtime.usage_summary("official").unwrap();
        assert_eq!(usage.turns, 2);
        let records = runtime.usage_records().unwrap();
        let official_rows: Vec<_> = records
            .iter()
            .filter(|record| record.route_id == "official")
            .collect();
        assert_eq!(official_rows.len(), 2);
        for record in &official_rows {
            assert_eq!(record.outcome.as_deref(), Some("success"));
            assert!(record.request_id.is_some(), "{record:?}");
            assert!(record.connection_id.is_some(), "{record:?}");
            let stages = record
                .stage_times_ms
                .as_ref()
                .expect("success rows must persist stage times");
            assert!(stages.get("clientAcceptMs").is_some(), "{stages}");
            assert!(stages.get("upstreamConnectedMs").is_some(), "{stages}");
            assert!(stages.get("requestForwardedMs").is_some(), "{stages}");
            assert!(
                stages.get("firstUpstreamApplicationFrameMs").is_some(),
                "{stages}"
            );
            assert!(stages.get("firstDownstreamFrameMs").is_some(), "{stages}");
            assert!(stages.get("terminalMs").is_some(), "{stages}");
        }

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_account_switch_replays_portable_history_on_new_account() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        fn account_hash(account_id: &str) -> String {
            format!("{:x}", Sha256::digest(account_id.as_bytes()))
        }

        fn jwt_for(account_id: &str) -> String {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(json!({"chatgpt_account_id": account_id}).to_string());
            format!("header.{payload}.signature")
        }

        fn select_account(root: &std::path::Path, account_id: &str, revision: u64) {
            std::fs::write(
                root.join("selected.json"),
                json!({
                    "accountIdHash": account_hash(account_id),
                    "selectionRevision": revision,
                    "selectionVerified": true
                })
                .to_string(),
            )
            .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let auth_root = temp.path().join("official-auth");
        for account_id in ["acct-a", "acct-b"] {
            let path = auth_root
                .join("grants")
                .join(format!("{}.json", account_hash(account_id)));
            crate::official_auth::write_grant_atomic(
                &path,
                &crate::official_auth::FileOfficialGrant {
                    account_id: account_id.into(),
                    access_token: jwt_for(account_id),
                    refresh_token: format!("refresh-{account_id}"),
                    expires_at_ms: chrono::Utc::now().timestamp_millis() + 600_000,
                },
            )
            .unwrap();
        }
        select_account(&auth_root, "acct-a", 1);

        #[derive(Clone, Default)]
        struct Capture {
            websocket_accounts: Arc<Mutex<Vec<String>>>,
            http_requests: Arc<Mutex<Vec<(String, Value)>>>,
        }

        let capture = Capture::default();
        let get_capture = capture.clone();
        let post_capture = capture.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
                let capture = get_capture.clone();
                async move {
                    capture.websocket_accounts.lock().unwrap().push(
                        headers
                            .get("chatgpt-account-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            let request: Value = serde_json::from_str(&text).unwrap();
                            if request.get("type").and_then(Value::as_str)
                                == Some("response.create")
                            {
                                socket
                                    .send(AxumWsMessage::Text(
                                        json!({
                                            "type": "response.completed",
                                            "response": {
                                                "id": "resp_account_a",
                                                "object": "response",
                                                "status": "completed",
                                                "output": [{
                                                    "type": "message",
                                                    "role": "assistant",
                                                    "content": [{
                                                        "type": "output_text",
                                                        "text": "answer from A"
                                                    }]
                                                }]
                                            }
                                        })
                                        .to_string()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                        }
                    })
                }
            })
            .post(
                move |headers: HeaderMap, axum::extract::Json(body): axum::extract::Json<Value>| {
                    let capture = post_capture.clone();
                    async move {
                        let account = headers
                            .get("chatgpt-account-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        capture.http_requests.lock().unwrap().push((account, body));
                        axum::Json(json!({
                            "id": "resp_account_b",
                            "object": "response",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "answer from B"}]
                            }]
                        }))
                    }
                },
            ),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let mut route =
            official_route_config(format!("http://{upstream_address}/backend-api/codex"));
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        route.credential_id = Some(crate::official_auth::SELECTED_OFFICIAL_CREDENTIAL_ID.into());
        let config = ProxyRuntimeConfig {
            data_dir: temp.path().to_path_buf(),
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "first question"}],
                    "stream": false
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(first) = first else {
            panic!("expected first completion");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&first).unwrap()["type"],
            "response.completed"
        );

        select_account(&auth_root, "acct-b", 2);
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "previous_response_id": "resp_account_a",
                    "input": [{"role": "user", "content": "follow-up question"}],
                    "stream": false
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let TungsteniteMessage::Text(text) = frame {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["type"] == "response.completed" {
                    assert_eq!(
                        event["response"]["id"],
                        "resp_account_b",
                        "websocket accounts={:?}, http requests={:?}",
                        capture.websocket_accounts.lock().unwrap(),
                        capture.http_requests.lock().unwrap()
                    );
                    break;
                }
            }
        }

        let websocket_accounts = capture.websocket_accounts.lock().unwrap().clone();
        assert_eq!(websocket_accounts, ["acct-a"]);
        let requests = capture.http_requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            1,
            "account handoff must use one portable POST"
        );
        assert_eq!(requests[0].0, "acct-b");
        assert!(
            requests[0].1.get("previous_response_id").is_none(),
            "old account response id must not reach the new account: {}",
            requests[0].1
        );
        let input_text = requests[0].1["input"].to_string();
        assert!(input_text.contains("first question"), "{input_text}");
        assert!(input_text.contains("answer from A"), "{input_text}");
        assert!(input_text.contains("follow-up question"), "{input_text}");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    fn official_route_config(base_url: String) -> RuntimeRouteConfig {
        RuntimeRouteConfig {
            route_id: "official".into(),
            catalog_id: "vellum-official".into(),
            name: "Official".into(),
            base_url,
            provider_kind: RuntimeProviderKind::Official,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "gpt-upstream".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }
    }

    #[tokio::test]
    async fn official_upstream_connect_disables_nagle() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(
                    |mut socket| async move { while let Some(Ok(_)) = socket.recv().await {} },
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let request = runtime_request_from_body(
            &state,
            &HeaderMap::new(),
            &json!({"type":"response.create","model":"vellum-official"}),
            "nodelay",
        );
        let plan = state
            .proxy_runtime()
            .prepare_official_websocket(&request)
            .await
            .unwrap()
            .expect("official plan");
        let handshake = official_upstream_handshake_request(&plan, &HeaderMap::new())
            .expect("official handshake request");
        let socket = connect_official_upstream(handshake)
            .await
            .expect("official connect");
        match socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => {
                assert!(
                    tcp.nodelay().expect("nodelay query"),
                    "Official upstream must disable Nagle so the first create leaves the host"
                );
            }
            other => panic!("expected plaintext test socket, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn send_official_upstream_flushes_the_first_create() {
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct RecordingSink {
            items: Vec<TungsteniteMessage>,
            flushes: usize,
        }

        impl futures_util::Sink<TungsteniteMessage> for RecordingSink {
            type Error = ();

            fn poll_ready(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn start_send(
                self: Pin<&mut Self>,
                item: TungsteniteMessage,
            ) -> Result<(), Self::Error> {
                self.get_mut().items.push(item);
                Ok(())
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                self.get_mut().flushes += 1;
                Poll::Ready(Ok(()))
            }

            fn poll_close(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
        }

        let mut sink = RecordingSink {
            items: Vec::new(),
            flushes: 0,
        };
        let message = TungsteniteMessage::Text(
            json!({"type":"response.create","model":"gpt-upstream"})
                .to_string()
                .into(),
        );
        send_official_upstream(&mut sink, message).await.unwrap();
        assert_eq!(sink.items.len(), 1, "first create must be written");
        assert!(
            sink.flushes >= 1,
            "first Official create must flush so rustls/TCP emit the frame"
        );
    }

    #[tokio::test]
    async fn official_first_create_reaches_delayed_upstream_before_a_second_client_frame() {
        let first_seen = Arc::new(tokio::sync::Notify::new());
        let first_seen_upstream = Arc::clone(&first_seen);
        let creates = Arc::new(AtomicUsize::new(0));
        let creates_upstream = Arc::clone(&creates);
        let extra_frames = Arc::new(AtomicUsize::new(0));
        let extra_frames_upstream = Arc::clone(&extra_frames);
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let first_seen_upstream = Arc::clone(&first_seen_upstream);
                let creates_upstream = Arc::clone(&creates_upstream);
                let extra_frames_upstream = Arc::clone(&extra_frames_upstream);
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await else {
                            return;
                        };
                        let value: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(value["type"], "response.create");
                        assert_eq!(value["model"], "gpt-upstream");
                        creates_upstream.fetch_add(1, Ordering::SeqCst);
                        first_seen_upstream.notify_one();
                        let completed = json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_first",
                                "object": "response",
                                "status": "completed",
                                "output": []
                            }
                        });
                        socket
                            .send(AxumWsMessage::Text(completed.to_string().into()))
                            .await
                            .unwrap();
                        if let Ok(Some(Ok(_))) = tokio::time::timeout(
                            std::time::Duration::from_millis(50),
                            socket.recv(),
                        )
                        .await
                        {
                            extra_frames_upstream.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let proxy =
            build_headless_router(Arc::new(state), InboundAccessPolicy::test_only_disabled());
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "first"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), first_seen.notified())
            .await
            .expect("delayed upstream must observe the first create without a second client frame");
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("reply after first create")
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = response else {
            panic!("expected response.completed");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["response"]["id"], "resp_first");
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            extra_frames.load(Ordering::SeqCst),
            0,
            "test must not send a second client frame while waiting for the first reply"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn run_bounded_aborts_the_spawned_task_on_timeout() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let result = run_bounded(
            async move {
                let _tx = tx;
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            },
            std::time::Duration::from_millis(20),
        )
        .await;
        assert!(matches!(result, Bounded::TimedOut));
        let closed = tokio::time::timeout(std::time::Duration::from_millis(500), rx).await;
        assert!(
            matches!(closed, Ok(Err(_))),
            "a timed-out connect must be aborted, not left running detached"
        );
    }

    #[tokio::test]
    async fn third_party_websocket_never_opens_an_official_upstream_connection() {
        use axum::extract::Json;
        use axum::routing::post;

        let official_connections = Arc::new(AtomicUsize::new(0));
        let official_connections_srv = Arc::clone(&official_connections);
        let official_upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let official_connections_srv = Arc::clone(&official_connections_srv);
                async move {
                    official_connections_srv.fetch_add(1, Ordering::SeqCst);
                    ws.on_upgrade(|mut socket| async move {
                        while let Some(Ok(_)) = socket.recv().await {}
                    })
                }
            }),
        );
        let official_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let official_address = official_listener.local_addr().unwrap();
        let official_task = tokio::spawn(async move {
            let _ = axum::serve(official_listener, official_upstream).await;
        });

        let third_party_hits = Arc::new(AtomicUsize::new(0));
        let third_party_hits_srv = Arc::clone(&third_party_hits);
        async fn responses(
            hits: Arc<AtomicUsize>,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            hits.fetch_add(1, Ordering::SeqCst);
            assert_eq!(body["model"], "third-party-upstream");
            let response = json!({
                "id": "resp_third_party",
                "object": "response",
                "status": "completed",
                "output": []
            });
            axum::Json(response).into_response()
        }
        let third_party_upstream = Router::new().route(
            "/v1/responses",
            post({
                let hits = Arc::clone(&third_party_hits_srv);
                move |body| responses(Arc::clone(&hits), body)
            }),
        );
        let third_party_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let third_party_address = third_party_listener.local_addr().unwrap();
        let third_party_task = tokio::spawn(async move {
            let _ = axum::serve(third_party_listener, third_party_upstream).await;
        });

        let official_route = RuntimeRouteConfig {
            route_id: "official".into(),
            catalog_id: "vellum-official".into(),
            name: "Official".into(),
            base_url: format!("http://{official_address}/backend-api/codex"),
            provider_kind: RuntimeProviderKind::Official,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "gpt-upstream".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let third_party_route = RuntimeRouteConfig {
            route_id: "third-party".into(),
            catalog_id: "vlm-third-party".into(),
            name: "ThirdParty".into(),
            base_url: format!("http://{third_party_address}/v1"),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: false,
            reasoning: true,
            vision: false,
            upstream_model: "third-party-upstream".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![official_route, third_party_route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-third-party",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": false
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = response else {
            panic!("expected a response.completed text frame");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["response"]["id"], "resp_third_party");

        assert_eq!(
            third_party_hits.load(Ordering::SeqCst),
            1,
            "the third-party route must be reached"
        );
        assert_eq!(
            official_connections.load(Ordering::SeqCst),
            0,
            "a third-party client WS must never open an Official upstream connection"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        official_task.abort();
        third_party_task.abort();
    }

    #[tokio::test]
    async fn concurrent_official_sessions_route_to_their_own_requested_model() {
        #[derive(Clone, Default)]
        struct Capture {
            seen_models: Arc<Mutex<Vec<String>>>,
        }
        let capture = Capture::default();
        let upstream_capture = capture.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let capture = upstream_capture.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await else {
                            return;
                        };
                        let value: Value = serde_json::from_str(&text).unwrap();
                        let model = value["model"].as_str().unwrap().to_string();
                        capture.seen_models.lock().unwrap().push(model.clone());
                        let completed = json!({
                            "type": "response.completed",
                            "response": {
                                "id": format!("resp_{model}"),
                                "object": "response",
                                "status": "completed",
                                "output": []
                            }
                        });
                        let _ = socket
                            .send(AxumWsMessage::Text(completed.to_string().into()))
                            .await;
                        while let Some(Ok(_)) = socket.recv().await {}
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        fn official_route(
            id: &str,
            catalog_id: &str,
            base_url: String,
            upstream_model: &str,
        ) -> RuntimeRouteConfig {
            RuntimeRouteConfig {
                route_id: id.into(),
                catalog_id: catalog_id.into(),
                name: id.into(),
                base_url,
                provider_kind: RuntimeProviderKind::Official,
                auth_kind: RuntimeAuthKind::None,
                wire: RuntimeWireFormat::Responses,
                server_side_resume: true,
                streaming: true,
                reasoning: true,
                vision: false,
                upstream_model: upstream_model.into(),
                context_window: Some(128_000),
                reasoning_capabilities: Default::default(),
                compaction_capabilities: Default::default(),
                compaction_policy: Default::default(),
                tool_capabilities: Default::default(),
                credential_id: None,
                catalog_entry: None,
                chat_capabilities: Default::default(),
                insecure_http_policy: Default::default(),
                access_mode: None,
            }
        }
        let config = ProxyRuntimeConfig {
            models: vec![
                official_route(
                    "official-a",
                    "vellum-official-a",
                    format!("http://{upstream_address}/backend-api/codex"),
                    "gpt-a",
                ),
                official_route(
                    "official-b",
                    "vellum-official-b",
                    format!("http://{upstream_address}/backend-api/codex"),
                    "gpt-b",
                ),
            ],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        async fn request_one(
            proxy_address: std::net::SocketAddr,
            catalog_id: &str,
            upstream_model: &str,
        ) {
            let (mut client, _) =
                tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                    .await
                    .unwrap();
            client
                .send(TungsteniteMessage::Text(
                    json!({
                        "type": "response.create",
                        "model": catalog_id,
                        "input": [{"role": "user", "content": upstream_model}],
                        "stream": true
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let TungsteniteMessage::Text(text) = response else {
                panic!("expected response.completed");
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                event["response"]["id"],
                format!("resp_{upstream_model}"),
                "session must resolve against its own requested model, not another session's"
            );
            let _ = client.close(None).await;
        }

        tokio::join!(
            request_one(proxy_address, "vellum-official-a", "gpt-a"),
            request_one(proxy_address, "vellum-official-b", "gpt-b"),
        );

        let seen = capture.seen_models.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.contains(&"gpt-a".to_string()));
        assert!(seen.contains(&"gpt-b".to_string()));

        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_upstream_ping_is_forwarded_on_the_reused_socket() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await else {
                        return;
                    };
                    let value: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(value["type"], "response.create");
                    socket
                        .send(AxumWsMessage::Ping(vec![9, 8, 7].into()))
                        .await
                        .unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    let completed = json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_ping",
                            "object": "response",
                            "status": "completed",
                            "output": []
                        }
                    });
                    let _ = socket
                        .send(AxumWsMessage::Text(completed.to_string().into()))
                        .await;
                    while let Some(Ok(_)) = socket.recv().await {}
                })
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let proxy =
            build_headless_router(Arc::new(state), InboundAccessPolicy::test_only_disabled());
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let mut saw_ping = false;
        let mut saw_completed = false;
        while !saw_completed {
            let message = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .expect("frame within deadline")
                .unwrap()
                .unwrap();
            match message {
                TungsteniteMessage::Ping(payload) => {
                    assert_eq!(payload.as_ref(), &[9, 8, 7][..]);
                    saw_ping = true;
                }
                TungsteniteMessage::Text(text) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(event["type"], "response.completed");
                    saw_completed = true;
                }
                other => panic!("unexpected frame on the reused socket: {other:?}"),
            }
        }
        assert!(
            saw_ping,
            "an upstream Ping on the single reused socket must reach the client"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_cancel_with_multiple_pending_turns_preserves_attribution() {
        let upstream_cancel_count = Arc::new(AtomicUsize::new(0));
        let upstream_cancel_count_for_route = Arc::clone(&upstream_cancel_count);
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let upstream_cancel_count = Arc::clone(&upstream_cancel_count_for_route);
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            let AxumWsMessage::Text(text) = message else {
                                if matches!(message, AxumWsMessage::Close(_)) {
                                    break;
                                }
                                continue;
                            };
                            let value: Value = serde_json::from_str(&text).unwrap();
                            if value.get("type").and_then(Value::as_str) == Some("response.cancel")
                            {
                                let cancel_count =
                                    upstream_cancel_count.fetch_add(1, Ordering::SeqCst) + 1;
                                if cancel_count == 2 {
                                    for id in ["resp_late_1", "resp_late_2"] {
                                        let completed = json!({
                                            "type": "response.completed",
                                            "response": {
                                                "id": id,
                                                "object": "response",
                                                "status": "completed",
                                                "output": []
                                            }
                                        });
                                        let _ = socket
                                            .send(AxumWsMessage::Text(completed.to_string().into()))
                                            .await;
                                    }
                                }
                            }
                        }
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hold"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hold-2"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "resp_pending"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "resp_pending"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "resp_pending_2"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime.usage_records().unwrap().iter().any(|record| {
                    record.route_id == "official"
                        && record.outcome.as_deref() == Some("client_cancel")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel must persist a usage row");
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(
            upstream_cancel_count.load(Ordering::SeqCst),
            2,
            "a repeated cancel for the tombstone must not be sent upstream"
        );
        let official: Vec<_> = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .filter(|record| record.route_id == "official")
            .collect();
        assert_eq!(
            official.len(),
            2,
            "both cancelled pending turns must be terminally accounted: {official:?}"
        );
        assert!(
            official
                .iter()
                .all(|record| record.outcome.as_deref() == Some("client_cancel")),
            "a tombstone terminal must not be attributed as success: {official:?}"
        );
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "resp_pending"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(
            upstream_cancel_count.load(Ordering::SeqCst),
            2,
            "a cancel arriving after the tombstone terminal must not be resent upstream"
        );
        let record = &official[0];
        assert_eq!(record.outcome.as_deref(), Some("client_cancel"));
        assert!(record.request_id.is_some(), "{record:?}");
        assert!(record.connection_id.is_some(), "{record:?}");
        assert_eq!(record.error_category.as_deref(), Some("client_cancel"));
        assert!(
            record
                .stage_times_ms
                .as_ref()
                .and_then(|value| value.get("terminalMs"))
                .is_some(),
            "{record:?}"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_silent_cancel_timeout_releases_pending_segment() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if matches!(message, AxumWsMessage::Close(_)) {
                            break;
                        }
                    }
                })
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        for content in ["silent-1", "silent-2"] {
            client
                .send(TungsteniteMessage::Text(
                    json!({
                        "type": "response.create",
                        "model": "vellum-official",
                        "input": [{"role": "user", "content": content}],
                        "stream": true
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel", "response_id": "silent-1"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let records = runtime
                    .usage_records()
                    .unwrap()
                    .into_iter()
                    .filter(|record| record.route_id == "official")
                    .collect::<Vec<_>>();
                if records.len() == 2 {
                    assert!(
                        records
                            .iter()
                            .any(|record| record.outcome.as_deref() == Some("client_cancel")),
                        "silent cancelled turn must be accounted: {records:?}"
                    );
                    assert!(
                        records.iter().any(|record| {
                            record.outcome.as_deref() == Some("client_disconnect")
                                && record.error_category.as_deref()
                                    == Some("segment_cancel_timeout")
                        }),
                        "remaining pending turn must fail closed on timeout: {records:?}"
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("silent upstream must not block the pending segment forever");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_client_close_records_disconnect_usage() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if matches!(message, AxumWsMessage::Close(_)) {
                            break;
                        }
                    }
                })
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hold"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = client.close(None).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime.usage_records().unwrap().iter().any(|record| {
                    record.route_id == "official"
                        && record.outcome.as_deref() == Some("client_disconnect")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client close must persist a disconnect usage row");
        let record = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .find(|record| record.outcome.as_deref() == Some("client_disconnect"))
            .unwrap();
        assert!(record.request_id.is_some(), "{record:?}");
        assert!(record.connection_id.is_some(), "{record:?}");
        assert!(record.stage_times_ms.is_some(), "{record:?}");

        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_handshake_failure_records_protocol_usage() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|| async { axum::http::StatusCode::BAD_REQUEST }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.next()).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime.usage_records().unwrap().iter().any(|record| {
                    record.route_id == "official"
                        && record.outcome.as_deref() == Some("protocol_failure")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("handshake failure must persist a usage row");
        let record = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .find(|record| record.outcome.as_deref() == Some("protocol_failure"))
            .unwrap();
        assert!(record.request_id.is_some(), "{record:?}");
        assert!(record.connection_id.is_some(), "{record:?}");
        assert_eq!(record.error_category.as_deref(), Some("provider_protocol"));

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_failed_frame_records_provider_failure() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        let AxumWsMessage::Text(text) = message else {
                            continue;
                        };
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value.get("type").and_then(Value::as_str) == Some("response.create") {
                            let failed = json!({
                                "type": "response.failed",
                                "response": {
                                    "status": "failed",
                                    "error": {
                                        "category": "provider_quota",
                                        "message": "quota exceeded"
                                    }
                                }
                            });
                            socket
                                .send(AxumWsMessage::Text(failed.to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                })
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.next()).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime.usage_records().unwrap().iter().any(|record| {
                    record.route_id == "official"
                        && record.outcome.as_deref() == Some("provider_failure")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("response.failed must persist a usage row");
        let record = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .find(|record| record.outcome.as_deref() == Some("provider_failure"))
            .unwrap();
        assert!(record.request_id.is_some(), "{record:?}");
        assert_eq!(record.error_category.as_deref(), Some("provider_quota"));
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("quota")),
            "{record:?}"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_websocket_top_level_error_is_terminal_invalid_request() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        let AxumWsMessage::Text(text) = message else {
                            continue;
                        };
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value.get("type").and_then(Value::as_str) == Some("response.create") {
                            socket
                                .send(AxumWsMessage::Text(
                                    json!({
                                        "type": "error",
                                        "status": 400,
                                        "error": {
                                            "type": "invalid_request_error",
                                            "message": "Invalid `previous_response_id`."
                                        }
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                        }
                    }
                })
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "continue"}],
                    "previous_response_id": "resp_from_other_account",
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = frame else {
            panic!("expected top-level error text, got {frame:?}");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "error");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime.usage_records().unwrap().iter().any(|record| {
                    record.outcome.as_deref() == Some("protocol_failure")
                        && record.error_category.as_deref() == Some("invalid_request")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("top-level error must terminate and persist the turn");
        let record = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .find(|record| record.error_category.as_deref() == Some("invalid_request"))
            .unwrap();
        assert_eq!(record.status, 400, "{record:?}");
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("previous_response_id")),
            "{record:?}"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn mid_turn_non_cancel_create_is_queued_and_processed() {
        use axum::body::{Body, Bytes};
        use axum::extract::Json;
        use axum::http::{header, StatusCode};
        use axum::routing::post;
        use std::convert::Infallible;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_route = Arc::clone(&seen);
        let upstream = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let seen_for_route = Arc::clone(&seen_for_route);
                async move {
                    let prompt = body
                        .pointer("/input/0/content")
                        .and_then(Value::as_str)
                        .or_else(|| body.pointer("/input/0/content/0/text").and_then(Value::as_str))
                        .unwrap_or("?")
                        .to_string();
                    seen_for_route.lock().unwrap().push(prompt.clone());
                    if prompt == "first" {
                        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
                    }
                    let stream = async_stream::stream! {
                        yield Ok::<Bytes, Infallible>(Bytes::from(format!(
                            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{prompt}\"}}\n\n"
                        )));
                        yield Ok(Bytes::from(format!(
                            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{prompt}\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}}}\n\n"
                        )));
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "compat".into(),
            catalog_id: "vlm-qwen".into(),
            name: "Qwen".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: "qwen3.8-27b".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-qwen",
                    "input": [{"role": "user", "content": "first"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-qwen",
                    "input": [{"role": "user", "content": "second"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let mut completed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while completed.len() < 2 && std::time::Instant::now() < deadline {
            let Some(Ok(TungsteniteMessage::Text(text))) = client.next().await else {
                continue;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["type"] == "response.completed" {
                completed.push(
                    event["response"]["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
        assert_eq!(
            completed,
            ["resp_first", "resp_second"],
            "second create must run after the in-flight turn, not be dropped"
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["first", "second"],
            "upstream must see both turns in order"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn third_party_websocket_forwards_the_first_sse_delta_before_terminal() {
        use axum::body::{Body, Bytes};
        use axum::extract::Json;
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use std::convert::Infallible;

        async fn responses(Json(body): Json<Value>) -> impl IntoResponse {
            assert_eq!(body["stream"], true);
            let stream = async_stream::stream! {
                yield Ok::<Bytes, Infallible>(Bytes::from(
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
                ));
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                yield Ok(Bytes::from(
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}\n\n",
                ));
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
        }

        let upstream = Router::new().route("/v1/responses", post(responses));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "compat".into(),
            catalog_id: "vlm-qwen".into(),
            name: "Qwen".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "qwen3.8-27b".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-qwen",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(first_text) = first else {
            panic!("expected a text frame, got {first:?}");
        };
        let first_event: Value = serde_json::from_str(&first_text).unwrap();
        assert_eq!(
            first_event["type"], "response.output_text.delta",
            "the first WebSocket frame must be the delta, not the terminal event: {first_event}"
        );

        let second = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(second_text) = second else {
            panic!("expected a completed frame");
        };
        let second_event: Value = serde_json::from_str(&second_text).unwrap();
        assert_eq!(second_event["type"], "response.completed");

        let records = runtime.usage_records().unwrap();
        assert!(
            records
                .iter()
                .any(|record| { record.route_id == "compat" && record.first_byte_ms.is_some() }),
            "third-party WebSocket usage must persist first_byte_ms: {records:?}"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn third_party_websocket_surfaces_http_426_as_response_failed() {
        use axum::http::StatusCode;
        use axum::routing::post;

        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async { StatusCode::from_u16(426).expect("426") }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "grok-cli".into(),
            catalog_id: "vlm-grok".into(),
            name: "Grok".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::GrokCli,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "grok-4.6".into(),
            context_window: Some(500_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-grok",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let failed = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("426 must fail the turn immediately")
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = failed else {
            panic!("expected response.failed text, got {failed:?}");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "response.failed");
        assert_eq!(event["response"]["error"]["category"], "provider_protocol");
        assert!(
            event["response"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("426")),
            "{event}"
        );
        let records = runtime.usage_records().unwrap();
        assert!(
            records.iter().any(|record| {
                record.route_id == "grok-cli" && record.status == 502 && record.error.is_some()
            }),
            "failed Grok turns must land in usage: {records:?}"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn buffered_websocket_frames_out_counts_each_successful_application_frame() {
        use crate::diagnostics::{DetailLevel, DiagnosticsSink};
        use axum::extract::Json;
        use axum::http::{header, StatusCode};
        use axum::routing::post;

        #[derive(Default)]
        struct RecordingSink {
            events: Mutex<Vec<DiagnosticEvent>>,
        }
        impl DiagnosticsSink for RecordingSink {
            fn record(&self, event: DiagnosticEvent) {
                self.events.lock().unwrap().push(event);
            }
            fn detail_level(&self) -> DetailLevel {
                DetailLevel::T1Summary
            }
        }

        let upstream = Router::new().route(
            "/v1/responses",
            post(|Json(_body): Json<Value>| async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    json!({
                        "id": "resp_buf",
                        "object": "response",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "hi"}]
                        }]
                    })
                    .to_string(),
                )
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "compat".into(),
            catalog_id: "vlm-qwen".into(),
            name: "Qwen".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: false,
            reasoning: false,
            vision: false,
            upstream_model: "qwen3.8-27b".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let sink = Arc::new(RecordingSink::default());
        let mut state = StaticProxyState::from_config_with_diagnostics(
            config,
            Arc::clone(&sink) as Arc<dyn DiagnosticsSink>,
        )
        .unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-qwen",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": false
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..8 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .expect("buffered frames must arrive")
                .unwrap()
                .unwrap();
            match frame {
                TungsteniteMessage::Ping(_) | TungsteniteMessage::Pong(_) => continue,
                TungsteniteMessage::Close(_) => break,
                TungsteniteMessage::Text(text) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    received.push(event["type"].as_str().unwrap_or("").to_string());
                    if event["type"] == "response.completed" || event["type"] == "response.failed" {
                        break;
                    }
                }
                other => panic!("unexpected control frame counted as application: {other:?}"),
            }
        }
        assert_eq!(
            received,
            [
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.output_item.done",
                "response.completed"
            ],
            "buffered replay must emit one frame per added/delta/done/completed: {received:?}"
        );

        let _ = client.close(None).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let closed = sink.events.lock().unwrap().iter().any(|event| {
                    matches!(event, DiagnosticEvent::WebSocketClosed(closed) if closed.frames_out == 5)
                });
                if closed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("WebSocketClosed must report frames_out == 5");

        let closed = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                DiagnosticEvent::WebSocketClosed(closed) => Some(closed.clone()),
                _ => None,
            })
            .expect("connection must emit WebSocketClosed");
        assert_eq!(
            closed.frames_out, 5,
            "framesOut must equal successful application sends, not 1: {closed:?}"
        );

        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn official_application_frame_excludes_control() {
        assert!(official_application_frame(&TungsteniteMessage::Text(
            "hi".into()
        )));
        assert!(!official_application_frame(&TungsteniteMessage::Ping(
            vec![].into()
        )));
        assert!(!official_application_frame(&TungsteniteMessage::Pong(
            vec![].into()
        )));
        assert!(!official_application_frame(&TungsteniteMessage::Close(
            None
        )));
    }

    #[tokio::test]
    async fn grok_chat_config_is_rewritten_and_hits_responses() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::AtomicBool;

        let hit_responses = Arc::new(AtomicBool::new(false));
        let hit_chat = Arc::new(AtomicBool::new(false));
        let responses_flag = Arc::clone(&hit_responses);
        let chat_flag = Arc::clone(&hit_chat);
        let upstream = Router::new()
            .route(
                "/v1/responses",
                post(move |Json(_body): Json<Value>| {
                    let responses_flag = Arc::clone(&responses_flag);
                    async move {
                        responses_flag.store(true, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            json!({
                                "id": "resp_grok",
                                "object": "response",
                                "status": "completed",
                                "output": [{
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{"type": "output_text", "text": "ok"}]
                                }]
                            })
                            .to_string(),
                        )
                    }
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move || {
                    let chat_flag = Arc::clone(&chat_flag);
                    async move {
                        chat_flag.store(true, Ordering::SeqCst);
                        StatusCode::IM_A_TEAPOT
                    }
                }),
            );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "grok-cli".into(),
            catalog_id: "vlm-grok".into(),
            name: "Grok".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::GrokCli,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Chat,
            server_side_resume: false,
            streaming: false,
            reasoning: true,
            vision: false,
            upstream_model: "grok-4.6".into(),
            context_window: Some(500_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        assert!(state.diagnostics().ready, "{:?}", state.diagnostics().notes);
        assert_eq!(state.config().models[0].wire, RuntimeWireFormat::Responses);
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-grok",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": false
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = first else {
            panic!("expected text, got {first:?}");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_ne!(event["type"], "response.failed", "{event}");
        assert!(
            hit_responses.load(Ordering::SeqCst),
            "Grok must call {{baseUrl}}/responses"
        );
        assert!(
            !hit_chat.load(Ordering::SeqCst),
            "Grok must not call /chat/completions"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn client_cancel_disconnects_mock_upstream_within_250ms() {
        use axum::body::{Body, Bytes};
        use axum::http::{header, StatusCode};
        use axum::routing::post;
        use std::convert::Infallible;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let seen_disconnect = Arc::new(AtomicUsize::new(0));
        let disconnect_flag = Arc::clone(&seen_disconnect);
        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let disconnect_flag = Arc::clone(&disconnect_flag);
                async move {
                    struct NotifyOnDrop(Arc<AtomicUsize>);
                    impl Drop for NotifyOnDrop {
                        fn drop(&mut self) {
                            self.0.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let stream = async_stream::stream! {
                        let _guard = NotifyOnDrop(disconnect_flag);
                        yield Ok::<Bytes, Infallible>(Bytes::from(
                            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                        ));
                        std::future::pending::<()>().await;
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "compat".into(),
            catalog_id: "vlm-qwen".into(),
            name: "Qwen".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Chat,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: "qwen3.8-27b".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-qwen",
                    "input": [{"role": "user", "content": "hi"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), client.next()).await;
        client
            .send(TungsteniteMessage::Text(
                json!({"type": "response.cancel"}).to_string().into(),
            ))
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while seen_disconnect.load(Ordering::SeqCst) == 0 {
            if std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            seen_disconnect.load(Ordering::SeqCst) >= 1,
            "mock upstream must observe disconnect within 250ms"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runtime.active_request_count(), 0);
        let records = runtime.usage_records().unwrap();
        let cancel_rows: Vec<_> = records
            .iter()
            .filter(|record| record.outcome.as_deref() == Some("client_cancel"))
            .collect();
        assert_eq!(
            cancel_rows.len(),
            1,
            "SSE cancel must persist exactly one usage row: {records:?}"
        );
        assert!(
            cancel_rows[0]
                .request_id
                .as_deref()
                .is_some_and(|id| id.starts_with("req_")),
            "cancel usage must use the real request_id: {:?}",
            cancel_rows[0]
        );
        assert!(
            cancel_rows[0].connection_id.is_some(),
            "{:?}",
            cancel_rows[0]
        );
        assert_eq!(cancel_rows[0].route_id, "compat");
        assert_eq!(cancel_rows[0].provider, "openAiCompatible");
        assert_eq!(cancel_rows[0].model, "qwen3.8-27b");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    /// P0 regression: a parent-tree cancel that lands on a *different*
    /// connection than the one an Official-native child is pending on used
    /// to only be noticed the next time the upstream happened to send a
    /// frame. Here the mock upstream never sends anything back after the
    /// handshake, so if `next_official_event` did not proactively race the
    /// pending turn's cancel watch, this would hang forever. The client on
    /// this connection never sends `response.cancel`, proving the
    /// termination is registry-driven, not wire-driven.
    #[tokio::test]
    async fn official_parent_tree_cancel_disconnects_a_silent_upstream_without_client_cancel() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dropped = Arc::new(AtomicUsize::new(0));
        let drop_flag = Arc::clone(&dropped);
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let drop_flag = Arc::clone(&drop_flag);
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        struct NotifyOnDrop(Arc<AtomicUsize>);
                        impl Drop for NotifyOnDrop {
                            fn drop(&mut self) {
                                self.0.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        let _guard = NotifyOnDrop(drop_flag);
                        // Silent upstream: never replies to response.create,
                        // simulating a provider that has stalled.
                        while let Some(Ok(_)) = socket.recv().await {}
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hold"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let execution_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(id) = runtime.debug_last_registered_execution_id() {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the turn must register a cancel socket before dispatch");

        // Cancel from "elsewhere": no `response.cancel` frame on this
        // connection, just the same registry API a different connection's
        // parent-tree cancel would call.
        runtime.cancel_request_tree(&execution_id);

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while dropped.load(Ordering::SeqCst) == 0 {
            if std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            dropped.load(Ordering::SeqCst) >= 1,
            "a parent-tree cancel must drop the silent upstream connection within 250ms, \
             with no client response.cancel frame and no upstream frame ever received"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime
                    .usage_records()
                    .unwrap()
                    .iter()
                    .any(|record| record.outcome.as_deref() == Some("client_cancel"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the proactively cancelled turn must record a client_cancel usage row");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    /// P1: when a parent-tree cancel forces a whole Official segment closed
    /// (no per-turn cancel address to target narrowly), the turn that was
    /// actually cancelled gets `client_cancel`, but any *other* turn
    /// pipelined on the same segment must not be silently dropped -- it gets
    /// `client_disconnect` categorized as `segment_closed_by_tree_cancel` so
    /// its usage is still accounted for.
    #[tokio::test]
    async fn sibling_pending_turn_gets_client_disconnect_when_tree_cancel_closes_the_segment() {
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(
                    |mut socket| async move { while let Some(Ok(_)) = socket.recv().await {} },
                )
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });
        let config = ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{upstream_address}/backend-api/codex"
            ))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();

        let turn_frame = || {
            TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "hold"}],
                    "stream": true
                })
                .to_string()
                .into(),
            )
        };
        client.send(turn_frame()).await.unwrap();
        let front_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(id) = runtime.debug_last_registered_execution_id() {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("front turn must register");

        client.send(turn_frame()).await.unwrap();
        let second_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(id) = runtime.debug_last_registered_execution_id() {
                    if id != front_id {
                        break id;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second turn must register on the reused segment");
        assert_ne!(front_id, second_id);

        runtime.cancel_request_tree(&front_id);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let records = runtime.usage_records().unwrap();
                if records.len() >= 2 {
                    break records;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both pending turns must be recorded once the segment is torn down");

        let records = runtime.usage_records().unwrap();
        assert_eq!(records.len(), 2, "{records:?}");
        let cancelled = records
            .iter()
            .find(|record| record.outcome.as_deref() == Some("client_cancel"))
            .unwrap_or_else(|| panic!("missing client_cancel row: {records:?}"));
        assert_eq!(cancelled.error_category.as_deref(), Some("client_cancel"));
        let sibling = records
            .iter()
            .find(|record| record.outcome.as_deref() == Some("client_disconnect"))
            .unwrap_or_else(|| panic!("missing client_disconnect row: {records:?}"));
        assert_eq!(
            sibling.error_category.as_deref(),
            Some("segment_closed_by_tree_cancel")
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn raw_request_frame_and_non_create_type_are_not_rewritten() {
        let raw = AxumWsMessage::Text(r#"{"model":"probe-model","input":"ping"}"#.into());
        assert_eq!(
            websocket_request_value(&raw).unwrap()["model"],
            "probe-model"
        );

        let other = AxumWsMessage::Text(r#"{"type":"session.update","model":"m"}"#.into());
        assert_eq!(
            websocket_request_value(&other).unwrap()["type"],
            "session.update"
        );
    }

    fn matrix_official_route(
        route_id: &str,
        catalog_id: &str,
        base_url: String,
        upstream_model: &str,
    ) -> RuntimeRouteConfig {
        RuntimeRouteConfig {
            route_id: route_id.into(),
            catalog_id: catalog_id.into(),
            // `name` (not `provider_kind`) is what usage rows attribute as
            // `provider`; keep it constant across the A/B routes so the
            // matrix tests can assert on it independent of route_id.
            name: "Official".into(),
            base_url,
            provider_kind: RuntimeProviderKind::Official,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: upstream_model.into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }
    }

    fn matrix_third_party_route(
        route_id: &str,
        catalog_id: &str,
        provider_kind: RuntimeProviderKind,
        base_url: String,
        upstream_model: &str,
    ) -> RuntimeRouteConfig {
        RuntimeRouteConfig {
            route_id: route_id.into(),
            catalog_id: catalog_id.into(),
            name: route_id.into(),
            base_url,
            provider_kind,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: false,
            vision: false,
            upstream_model: upstream_model.into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }
    }

    /// Verification matrix: same client WebSocket, Official A -> Official B.
    /// Both routes are account-compatible (same upstream, no auth), so the
    /// second turn must reuse the socket the first turn opened — only the
    /// upstream model changes — and each turn's usage row must attribute to
    /// its own route/model/request id, never the other turn's.
    #[tokio::test]
    async fn official_a_to_official_b_reuses_one_socket_and_attributes_each_turn() {
        #[derive(Clone, Default)]
        struct Capture {
            connections: Arc<AtomicUsize>,
            models_seen: Arc<Mutex<Vec<String>>>,
        }
        let capture = Capture::default();
        let upstream_capture = capture.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let capture = upstream_capture.clone();
                async move {
                    capture.connections.fetch_add(1, Ordering::SeqCst);
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            let value: Value = serde_json::from_str(&text).unwrap();
                            if value.get("type").and_then(Value::as_str) == Some("response.create")
                            {
                                let model = value["model"].as_str().unwrap().to_string();
                                capture.models_seen.lock().unwrap().push(model.clone());
                                let completed = json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": format!("resp_{model}"),
                                        "object": "response",
                                        "status": "completed",
                                        "output": []
                                    }
                                });
                                let _ = socket
                                    .send(AxumWsMessage::Text(completed.to_string().into()))
                                    .await;
                            }
                        }
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let base = format!("http://{upstream_address}/backend-api/codex");
        let config = ProxyRuntimeConfig {
            models: vec![
                matrix_official_route("official-a", "vellum-official-a", base.clone(), "gpt-a"),
                matrix_official_route("official-b", "vellum-official-b", base, "gpt-b"),
            ],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();

        for (catalog_id, upstream_model) in [
            ("vellum-official-a", "gpt-a"),
            ("vellum-official-b", "gpt-b"),
        ] {
            client
                .send(TungsteniteMessage::Text(
                    json!({
                        "type": "response.create",
                        "model": catalog_id,
                        "input": [{"role": "user", "content": "hi"}],
                        "stream": true
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let TungsteniteMessage::Text(text) = response else {
                panic!("expected response.completed");
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["response"]["id"], format!("resp_{upstream_model}"));
        }

        assert_eq!(
            capture.connections.load(Ordering::SeqCst),
            1,
            "Official A -> Official B on one client socket must reuse the upstream connection"
        );
        assert_eq!(
            capture.models_seen.lock().unwrap().as_slice(),
            &["gpt-a".to_string(), "gpt-b".to_string()]
        );

        let records = runtime.usage_records().unwrap();
        let a = records
            .iter()
            .find(|record| record.model == "gpt-a")
            .expect("gpt-a usage row");
        let b = records
            .iter()
            .find(|record| record.model == "gpt-b")
            .expect("gpt-b usage row");
        assert_eq!(a.route_id, "official-a");
        assert_eq!(b.route_id, "official-b");
        assert_eq!(a.provider, "Official");
        assert_eq!(b.provider, "Official");
        assert_ne!(
            a.request_id, b.request_id,
            "each turn keeps its own request id"
        );
        assert_eq!(a.outcome.as_deref(), Some("success"));
        assert_eq!(b.outcome.as_deref(), Some("success"));

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    /// A `response.create` that would move off the currently open Official
    /// segment's route while a previous turn on it is still in flight must be
    /// rejected (fail closed), never silently reordered onto a different
    /// transport, and must not disturb the turn already in flight.
    #[tokio::test]
    async fn mid_flight_provider_switch_is_rejected_while_official_turn_pending() {
        let release = Arc::new(tokio::sync::Notify::new());
        let release_for_route = Arc::clone(&release);
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let release = Arc::clone(&release_for_route);
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        let Some(Ok(AxumWsMessage::Text(_))) = socket.recv().await else {
                            return;
                        };
                        release.notified().await;
                        let completed = json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_official_1",
                                "object": "response",
                                "status": "completed",
                                "output": []
                            }
                        });
                        let _ = socket
                            .send(AxumWsMessage::Text(completed.to_string().into()))
                            .await;
                        while socket.recv().await.is_some() {}
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let compat_hits = Arc::new(AtomicUsize::new(0));
        let compat_hits_for_route = Arc::clone(&compat_hits);
        let compat_upstream = Router::new().route(
            "/v1/responses",
            axum::routing::post(move || {
                let compat_hits = Arc::clone(&compat_hits_for_route);
                async move {
                    compat_hits.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::IM_A_TEAPOT
                }
            }),
        );
        let compat_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let compat_address = compat_listener.local_addr().unwrap();
        let compat_task = tokio::spawn(async move {
            let _ = axum::serve(compat_listener, compat_upstream).await;
        });

        let config = ProxyRuntimeConfig {
            models: vec![
                matrix_official_route(
                    "official",
                    "vellum-official",
                    format!("http://{upstream_address}/backend-api/codex"),
                    "gpt-upstream",
                ),
                matrix_third_party_route(
                    "compat",
                    "vlm-compat",
                    RuntimeProviderKind::OpenAiCompatible,
                    format!("http://{compat_address}/v1"),
                    "compat-model",
                ),
            ],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();

        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "official turn"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        // Give the proxy time to open the Official segment and forward the
        // first turn upstream before attempting the mid-flight switch.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vlm-compat",
                    "input": [{"role": "user", "content": "switch attempt"}],
                    "stream": true
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let rejection = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = rejection else {
            panic!("expected a response.failed rejection frame");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "response.failed");
        let message = event["response"]["error"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains("in flight"),
            "rejection must explain the fail-closed reason: {message}"
        );

        release.notify_one();
        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = completed else {
            panic!("expected the in-flight Official turn to still complete");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["response"]["id"], "resp_official_1");

        assert_eq!(
            compat_hits.load(Ordering::SeqCst),
            0,
            "the rejected turn must never reach the third-party upstream"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
        compat_task.abort();
    }

    /// An unknown model fails only that turn (never falls back to a previous
    /// turn's route) and must not disturb an Official segment that is
    /// already open and idle.
    #[tokio::test]
    async fn unknown_model_fails_the_turn_without_disturbing_the_open_segment() {
        #[derive(Clone, Default)]
        struct Capture {
            connections: Arc<AtomicUsize>,
        }
        let capture = Capture::default();
        let upstream_capture = capture.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let capture = upstream_capture.clone();
                async move {
                    capture.connections.fetch_add(1, Ordering::SeqCst);
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            let value: Value = serde_json::from_str(&text).unwrap();
                            if value.get("type").and_then(Value::as_str) == Some("response.create")
                            {
                                let completed = json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": "resp_official",
                                        "object": "response",
                                        "status": "completed",
                                        "output": []
                                    }
                                });
                                let _ = socket
                                    .send(AxumWsMessage::Text(completed.to_string().into()))
                                    .await;
                            }
                        }
                    })
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let config = ProxyRuntimeConfig {
            models: vec![matrix_official_route(
                "official",
                "vellum-official",
                format!("http://{upstream_address}/backend-api/codex"),
                "gpt-upstream",
            )],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();

        let turn = |model: &str, content: &str| {
            TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": model,
                    "input": [{"role": "user", "content": content}],
                    "stream": true
                })
                .to_string()
                .into(),
            )
        };

        client.send(turn("vellum-official", "one")).await.unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = first else {
            panic!("expected response.completed");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["response"]["id"],
            "resp_official"
        );

        client
            .send(turn("model-that-does-not-exist", "two"))
            .await
            .unwrap();
        let failure = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = failure else {
            panic!("expected response.failed");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "response.failed");

        client.send(turn("vellum-official", "three")).await.unwrap();
        let third = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = third else {
            panic!("expected response.completed");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["response"]["id"],
            "resp_official"
        );

        assert_eq!(
            capture.connections.load(Ordering::SeqCst),
            1,
            "an unknown model between two known-good turns must not tear down the open segment"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    /// Verification matrix: Official -> Grok -> OpenCode -> Official on one
    /// client socket. Every switch away from Official closes the native
    /// segment and runs the shared portable execute() path (never forwarding
    /// Official's encrypted reasoning or `previous_response_id` verbatim);
    /// coming back to Official with a fresh turn opens a new native segment.
    #[tokio::test]
    async fn official_grok_opencode_official_switches_transport_and_leaks_nothing() {
        let official_connections = Arc::new(AtomicUsize::new(0));
        let official_connections_for_route = Arc::clone(&official_connections);
        let official_upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let connections = Arc::clone(&official_connections_for_route);
                async move {
                    connections.fetch_add(1, Ordering::SeqCst);
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            let value: Value = serde_json::from_str(&text).unwrap();
                            if value.get("type").and_then(Value::as_str) == Some("response.create")
                            {
                                let completed = json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": "resp_official_1",
                                        "object": "response",
                                        "status": "completed",
                                        "output": [
                                            {
                                                "type": "reasoning",
                                                "id": "rs_1",
                                                "encrypted_content": "opaque-secret-reasoning"
                                            },
                                            {
                                                "type": "message",
                                                "id": "msg_1",
                                                "role": "assistant",
                                                "status": "completed",
                                                "content": [{"type": "output_text", "text": "hi from official"}]
                                            }
                                        ]
                                    }
                                });
                                let _ = socket
                                    .send(AxumWsMessage::Text(completed.to_string().into()))
                                    .await;
                            }
                        }
                    })
                }
            }),
        );
        let official_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let official_address = official_listener.local_addr().unwrap();
        let official_task = tokio::spawn(async move {
            let _ = axum::serve(official_listener, official_upstream).await;
        });

        let grok_bodies: Arc<Mutex<Vec<Bytes>>> = Arc::new(Mutex::new(Vec::new()));
        let grok_headers: Arc<Mutex<Vec<HeaderMap>>> = Arc::new(Mutex::new(Vec::new()));
        let grok_bodies_for_route = Arc::clone(&grok_bodies);
        let grok_headers_for_route = Arc::clone(&grok_headers);
        let grok_upstream = Router::new().route(
            "/v1/responses",
            axum::routing::post(move |headers: HeaderMap, body: Bytes| {
                let grok_bodies = Arc::clone(&grok_bodies_for_route);
                let grok_headers = Arc::clone(&grok_headers_for_route);
                async move {
                    grok_bodies.lock().unwrap().push(body);
                    grok_headers.lock().unwrap().push(headers);
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id": "resp_grok_1",
                            "object": "response",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "hi from grok"}]
                            }]
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let grok_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grok_address = grok_listener.local_addr().unwrap();
        let grok_task = tokio::spawn(async move {
            let _ = axum::serve(grok_listener, grok_upstream).await;
        });

        let opencode_bodies: Arc<Mutex<Vec<Bytes>>> = Arc::new(Mutex::new(Vec::new()));
        let opencode_bodies_for_route = Arc::clone(&opencode_bodies);
        let opencode_upstream = Router::new().route(
            "/v1/responses",
            axum::routing::post(move |body: Bytes| {
                let opencode_bodies = Arc::clone(&opencode_bodies_for_route);
                async move {
                    opencode_bodies.lock().unwrap().push(body);
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id": "resp_opencode_1",
                            "object": "response",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "hi from opencode"}]
                            }]
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let opencode_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let opencode_address = opencode_listener.local_addr().unwrap();
        let opencode_task = tokio::spawn(async move {
            let _ = axum::serve(opencode_listener, opencode_upstream).await;
        });

        let config = ProxyRuntimeConfig {
            models: vec![
                matrix_official_route(
                    "official",
                    "vellum-official",
                    format!("http://{official_address}/backend-api/codex"),
                    "gpt-upstream",
                ),
                matrix_third_party_route(
                    "grok",
                    "vellum-grok",
                    RuntimeProviderKind::GrokCli,
                    format!("http://{grok_address}/v1"),
                    "grok-4.5",
                ),
                matrix_third_party_route(
                    "opencode",
                    "vellum-opencode",
                    RuntimeProviderKind::OpenAiCompatible,
                    format!("http://{opencode_address}/v1"),
                    "opencode-model",
                ),
            ],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(
            Arc::clone(&state),
            InboundAccessPolicy::test_only_disabled(),
        );
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, proxy).await;
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{proxy_address}/v1/responses"))
                .await
                .unwrap();

        async fn send_and_read(
            client: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            body: Value,
        ) -> Value {
            client
                .send(TungsteniteMessage::Text(body.to_string().into()))
                .await
                .unwrap();
            // The native Official path emits exactly one application frame
            // per completed turn in these fixtures, but the portable
            // (buffered-response) path replays output_item/output_text
            // frames before the terminal response.completed — keep reading
            // until the terminal event arrives.
            loop {
                let response =
                    tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                let TungsteniteMessage::Text(text) = response else {
                    panic!("expected a text frame");
                };
                let event: Value = serde_json::from_str(&text).unwrap();
                if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                    return event;
                }
            }
        }

        // Turn 1: Official, native WebSocket.
        let first = send_and_read(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vellum-official",
                "input": [{"role": "user", "content": "hello"}],
                "stream": true
            }),
        )
        .await;
        assert_eq!(first["response"]["id"], "resp_official_1");

        // Turn 2: Official -> Grok. Continues the Official conversation, so
        // the portable path must sanitize away the encrypted reasoning and
        // strip previous_response_id before this ever reaches Grok.
        let second = send_and_read(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vellum-grok",
                "previous_response_id": "resp_official_1",
                "input": [{"role": "user", "content": "continue on grok"}],
                "stream": false
            }),
        )
        .await;
        assert_eq!(second["response"]["id"], "resp_grok_1");

        // Turn 3: Grok -> OpenCode (third-party to third-party).
        let third = send_and_read(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vellum-opencode",
                "previous_response_id": "resp_grok_1",
                "input": [{"role": "user", "content": "continue on opencode"}],
                "stream": false
            }),
        )
        .await;
        assert_eq!(third["response"]["id"], "resp_opencode_1");

        // Turn 4: back to Official as a fresh turn (no previous_response_id)
        // — must open a brand-new native segment, not resurrect anything.
        let fourth = send_and_read(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vellum-official",
                "input": [{"role": "user", "content": "back on official"}],
                "stream": true
            }),
        )
        .await;
        assert_eq!(fourth["response"]["id"], "resp_official_1");

        assert_eq!(
            official_connections.load(Ordering::SeqCst),
            2,
            "leaving and returning to Official must close the old segment and open a fresh one"
        );

        let grok_body = grok_bodies.lock().unwrap()[0].clone();
        let grok_body_text = String::from_utf8_lossy(&grok_body).to_string();
        assert!(
            !grok_body_text.contains("opaque-secret-reasoning"),
            "Official's encrypted reasoning must never reach a third-party upstream: {grok_body_text}"
        );
        assert!(
            !grok_body_text.contains("previous_response_id"),
            "previous_response_id must never be forwarded to a third-party upstream: {grok_body_text}"
        );
        let grok_header = grok_headers.lock().unwrap()[0].clone();
        assert!(
            !grok_header.contains_key("authorization"),
            "Grok request must not carry Official's (or anyone else's) auth header: {grok_header:?}"
        );

        let opencode_body = opencode_bodies.lock().unwrap()[0].clone();
        let opencode_body_text = String::from_utf8_lossy(&opencode_body).to_string();
        assert!(
            !opencode_body_text.contains("opaque-secret-reasoning"),
            "encrypted reasoning must not leak across a Grok -> OpenCode switch either: {opencode_body_text}"
        );
        assert!(!opencode_body_text.contains("previous_response_id"));

        let records = runtime.usage_records().unwrap();
        let grok_row = records
            .iter()
            .find(|record| record.route_id == "grok")
            .expect("grok usage row");
        assert_eq!(grok_row.provider, "grokCli");
        let opencode_row = records
            .iter()
            .find(|record| record.route_id == "opencode")
            .expect("opencode usage row");
        assert_eq!(opencode_row.provider, "openAiCompatible");
        let official_rows: Vec<_> = records
            .iter()
            .filter(|record| record.route_id == "official")
            .collect();
        assert_eq!(
            official_rows.len(),
            2,
            "both Official turns (before and after the third-party detour) must be recorded"
        );

        let _ = client.close(None).await;
        proxy_task.abort();
        official_task.abort();
        grok_task.abort();
        opencode_task.abort();
    }
}
