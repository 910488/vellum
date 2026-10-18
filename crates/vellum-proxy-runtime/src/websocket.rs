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

/// A full-input `response.create` (no `previous_response_id`) larger than this
/// is served over the HTTP Responses path instead of the upstream WebSocket.
/// chatgpt.com closes the socket right after a ~27 MB first frame without any
/// error frame (2026-10-18, thread resume), while the same body succeeds over
/// HTTP (up to ~30 MB observed). Override with
/// `VELLUM_OFFICIAL_WS_MAX_FRAME_BYTES` (0 disables the redirect).
const OFFICIAL_WS_FULL_INPUT_HTTP_THRESHOLD_BYTES: usize = 16 * 1024 * 1024;

fn official_ws_full_input_threshold() -> usize {
    std::env::var("VELLUM_OFFICIAL_WS_MAX_FRAME_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(OFFICIAL_WS_FULL_INPUT_HTTP_THRESHOLD_BYTES)
}

fn has_previous_response_id(request: &Value) -> bool {
    request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// Encoded size of a full-input frame that exceeds `threshold`, else `None`.
/// Chained frames (carrying `previous_response_id`) are incremental and never
/// qualify.
fn oversize_full_input_frame_bytes(request: &Value, threshold: usize) -> Option<usize> {
    if threshold == 0 || has_previous_response_id(request) {
        return None;
    }
    let bytes = request.to_string().len();
    (bytes > threshold).then_some(bytes)
}

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
    quota_retried: bool,
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
                execution_identity: Some(current),
            },
            crate::exec::OfficialWebSocketAuthPosture::Managed {
                execution_identity: Some(next),
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
    let mut previous_metadata: Option<crate::request::RequestMetadata> = None;
    let mut queued: VecDeque<Value> = VecDeque::new();
    let mut deferred_official: VecDeque<DeferredOfficialTurn> = VecDeque::new();
    let mut quota_retries: VecDeque<DeferredOfficialTurn> = VecDeque::new();
    let mut deferred_account_handoffs: VecDeque<Value> = VecDeque::new();
    let mut cancelled_response_ids: VecDeque<String> = VecDeque::new();
    let mut portable_warmup: Option<PortableWarmup> = None;
    // Ids of responses created over HTTP (oversize full-input redirect). A
    // chained follow-up naming one must not go to an upstream socket, which
    // has never seen that id, whether or not a socket is currently open.
    let mut http_response_ids: VecDeque<String> = VecDeque::new();
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
                if let Some(turn) = quota_retries.pop_front() {
                    break WebSocketWork::Deferred(turn);
                }
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
                            if let Some(turn) = prepare_quota_retry(
                                &state, &headers, runtime.as_ref(), &mut official,
                                &connection_id, upstream_message.as_ref(),
                            ).await {
                                quota_retries.push_back(turn);
                                continue;
                            }
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

        let (
            mut request,
            preplanned_runtime_request,
            preplanned_plan,
            preplanned_execution_id,
            mut quota_retried,
        ) = match work {
            WebSocketWork::New(request) => (request, None, None, None, false),
            WebSocketWork::Deferred(turn) => (
                turn.request,
                Some(turn.runtime_request),
                Some(turn.plan),
                Some(turn.execution_id),
                turn.quota_retried,
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
                // Warmup requests do not generate inference. Never forward them
                // into the native pending queue, where an unanswered warmup can
                // otherwise live until disconnect and become a phantom failure.
                if request.get("generate").and_then(Value::as_bool) == Some(false) {
                    if official
                        .as_ref()
                        .is_some_and(|segment| !segment.pending.is_empty())
                    {
                        // Preserve terminal ordering: finish the running turn
                        // before emitting the local warmup completion.
                        deferred_account_handoffs.push_back(request);
                        continue;
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
                        false,
                        &mut portable_warmup,
                        None,
                        &mut None,
                    )
                    .await
                    {
                        TurnOutcome::Continue => {}
                        TurnOutcome::Stop(stop_reason) => {
                            reason = stop_reason;
                            break;
                        }
                    }
                    continue;
                }
                turns += 1;
                if preplanned_runtime_request.is_none() {
                    resume_portable_warmup(&mut request, &mut portable_warmup);
                }
                let was_deferred = preplanned_plan.is_some();
                let mut runtime_request = preplanned_runtime_request.clone().unwrap_or_else(|| {
                    runtime_request_from_body(&state, &headers, &request, &connection_id)
                });
                let continuation_context = previous_metadata
                    .clone()
                    .or_else(|| runtime.official_continuation_context(&runtime_request));
                apply_socket_affinity(
                    &mut runtime_request,
                    continuation_context.as_ref(),
                    official
                        .as_ref()
                        .and_then(|segment| segment.execution_account_hash.as_deref()),
                    official
                        .as_ref()
                        .map_or(0, |segment| segment.last_input_tokens),
                    official
                        .as_ref()
                        .is_some_and(|segment| segment.quota_exhausted),
                );
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
                let planned_account_hash = match &turn_dispatch {
                    WebSocketTurnDispatch::OfficialNative(plan) => {
                        plan.execution_account_hash.clone()
                    }
                    _ => None,
                };
                let mut track_http_response_id = false;
                if matches!(&turn_dispatch, WebSocketTurnDispatch::OfficialNative(_)) {
                    if let Some(bytes) = oversize_full_input_frame_bytes(
                        &request,
                        official_ws_full_input_threshold(),
                    ) {
                        // The upstream socket drops oversized first frames
                        // and Codex burns minutes retrying before its own
                        // HTTP fallback. Serve this turn over HTTP now; the
                        // body is untouched (no handoff sanitising), so the
                        // session-id/prompt_cache_key affinity is preserved.
                        log::warn!(
                            "[OfficialSocket] full-input frame {bytes} bytes exceeds the WebSocket limit; serving over HTTP connection={connection_id}"
                        );
                        turn_dispatch = WebSocketTurnDispatch::PortableReplay {
                            force_official_handoff: false,
                        };
                        track_http_response_id = true;
                    } else if request
                        .get("previous_response_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| http_response_ids.iter().any(|known| known == id))
                    {
                        // The previous response was created over HTTP, so a
                        // fresh upstream socket cannot resume it. Replay the
                        // full history over HTTP as well.
                        log::info!(
                            "[OfficialSocket] chained frame follows an HTTP-served turn; replaying over HTTP connection={connection_id}"
                        );
                        runtime_request.metadata.force_portable_official_handoff = true;
                        turn_dispatch = WebSocketTurnDispatch::PortableReplay {
                            force_official_handoff: true,
                        };
                        track_http_response_id = true;
                    }
                }
                let live_account_handoff = matches!(
                    &turn_dispatch,
                    WebSocketTurnDispatch::OfficialNative(plan)
                        if plan.cross_realm_handoff
                            || request.get("previous_response_id").and_then(Value::as_str).is_some()
                            && official.as_ref().is_some_and(|segment| {
                                managed_account_changed(
                                    &segment.connection_key,
                                    &plan.connection_key,
                                ) || (segment.selection_revision != plan.selection_revision
                                    && !runtime_request.metadata.official_socket_affinity.as_ref().is_some_and(|affinity| affinity.turn_in_progress))
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
                    let hydrated = match &turn_dispatch {
                        WebSocketTurnDispatch::OfficialNative(plan) => {
                            runtime.hydrate_official_socket_handoff(plan, &request)
                        }
                        WebSocketTurnDispatch::PortableReplay { .. } => {
                            unreachable!("live handoff is only planned for native turns")
                        }
                    };
                    match hydrated {
                        Ok(body) => {
                            log::info!(
                                "[OfficialSocket] account handoff replayed on socket connection={connection_id} input_items={}",
                                crate::request::request_input_items(&body).len()
                            );
                            // Only a chained frame can be retried as full
                            // input by Codex; that retry carries no
                            // previous_response_id and is never marked.
                            if request
                                .get("previous_response_id")
                                .and_then(Value::as_str)
                                .is_some()
                            {
                                if let WebSocketTurnDispatch::OfficialNative(plan) =
                                    &mut turn_dispatch
                                {
                                    plan.handoff_replay =
                                        Some(handoff_replay_summary(&request, &body));
                                }
                            }
                            request = body;
                            runtime_request.body = request.clone();
                        }
                        Err(error) => {
                            // Missing local history keeps the portable path,
                            // which asks Codex for a full-input retry.
                            log::warn!(
                                "[OfficialSocket] account handoff falls back to portable replay connection={connection_id} kind={}",
                                error.category()
                            );
                            runtime_request.metadata.force_portable_official_handoff = true;
                            turn_dispatch = WebSocketTurnDispatch::PortableReplay {
                                force_official_handoff: true,
                            };
                        }
                    }
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
                        if reuse
                            || official
                                .as_ref()
                                .is_none_or(|segment| segment.pending.is_empty())
                        {
                            previous_metadata = Some(runtime_request.metadata.clone());
                        }
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
                                quota_retried,
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
                                quota_retried,
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
                                &mut runtime_request,
                                &mut request,
                                &mut quota_retried,
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
                                        quota_retried,
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
                        if let Some(hash) = planned_account_hash {
                            // The shared HTTP executor resolves auth again. Pin
                            // that resolution to the already planned account,
                            // and retain the new binding for later increments.
                            runtime_request.metadata.official_socket_affinity =
                                Some(crate::request::OfficialSocketAffinity {
                                    account_hash: hash
                                        .strip_prefix("sha256:")
                                        .unwrap_or(&hash)
                                        .to_owned(),
                                    turn_in_progress: true,
                                    input_tokens: runtime_request
                                        .metadata
                                        .official_socket_affinity
                                        .as_ref()
                                        .map_or(0, |a| a.input_tokens),
                                    quota_exhausted: false,
                                });
                        }
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
                        previous_metadata = Some(runtime_request.metadata.clone());
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
                        let mut produced_response_id: Option<String> = None;
                        let portable_outcome = execute_portable_turn(
                            &mut client,
                            &state,
                            runtime.as_ref(),
                            &headers,
                            request,
                            &connection_id,
                            &mut queued,
                            &mut frames_out,
                            force_official_handoff,
                            &mut portable_warmup,
                            Some(runtime_request),
                            &mut produced_response_id,
                        )
                        .await;
                        if track_http_response_id {
                            if let Some(id) = produced_response_id {
                                http_response_ids.push_back(id);
                                while http_response_ids.len() > 8 {
                                    http_response_ids.pop_front();
                                }
                            }
                        }
                        match portable_outcome {
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

    for turn in quota_retries.drain(..).chain(deferred_official.drain(..)) {
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
    execution_account_hash: Option<String>,
    last_input_tokens: u64,
    quota_exhausted: bool,
    selection_revision: Option<u64>,
    connection_key: OfficialWebSocketConnectionKey,
    write: futures_util::stream::SplitSink<OfficialUpstreamSocket, TungsteniteMessage>,
    read: futures_util::stream::SplitStream<OfficialUpstreamSocket>,
    pending: VecDeque<OfficialPendingTurn>,
    stages: OfficialStageTimes,
    started: std::time::Instant,
    /// Encoded size of the most recent frame forwarded upstream, kept so an
    /// upstream close can be logged next to the frame that provoked it.
    last_sent_frame_bytes: usize,
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
    runtime_request: &mut RuntimeRequest,
    frame: &mut Value,
    quota_retried: &mut bool,
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
            Bounded::Ready(Err(error))
                if matches!(error.as_ref(), tokio_tungstenite::tungstenite::Error::Http(response)
                    if response.status().as_u16() == 429) =>
            {
                let tokio_tungstenite::tungstenite::Error::Http(response) = *error else {
                    unreachable!()
                };
                let reset = response
                    .body()
                    .as_deref()
                    .and_then(|body| serde_json::from_slice::<Value>(body).ok())
                    .and_then(|body| quota_failure_reset(&body));
                if let Some(next) =
                    prepare_quota_handoff(runtime, plan, runtime_request, reset, !*quota_retried)
                        .await
                {
                    *frame = runtime_request.body.clone();
                    *plan = next;
                    *quota_retried = true;
                    refreshed = false;
                    continue;
                }
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
            Bounded::Ready(Err(error))
                if !refreshed
                    && matches!(
                        error.as_ref(),
                        tokio_tungstenite::tungstenite::Error::Http(response)
                            if matches!(response.status().as_u16(), 401 | 403)
                    ) =>
            {
                let tokio_tungstenite::tungstenite::Error::Http(response) = *error else {
                    unreachable!("guard requires an HTTP handshake error")
                };
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
            Bounded::Ready(Err(error)) => {
                let error = match *error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        let error = websocket_handshake_error(response);
                        tracing::warn!(%error, "Official WebSocket handshake rejected");
                        error
                    }
                    error => RuntimeError::ProviderUnavailable(format!(
                        "Official WebSocket connection failed: {error}"
                    )),
                };
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
        execution_account_hash: plan.execution_account_hash.clone(),
        last_input_tokens: 0,
        quota_exhausted: false,
        selection_revision: plan.selection_revision,
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
        last_sent_frame_bytes: 0,
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
    quota_retried: bool,
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
    if let TungsteniteMessage::Text(text) = &forwarded {
        segment.last_sent_frame_bytes = text.len();
    }
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
    segment.selection_revision = plan.selection_revision;
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
    turn.quota_retried = quota_retried;
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

fn quota_failure_reset(failed: &Value) -> Option<u64> {
    failed
        .pointer("/response/error/resets_at")
        .or_else(|| failed.pointer("/error/resets_at"))
        .and_then(Value::as_u64)
        .map(|seconds| seconds.saturating_mul(1000))
}

async fn prepare_quota_handoff(
    runtime: &ProxyRuntime,
    plan: &mut OfficialWebSocketPlan,
    request: &mut RuntimeRequest,
    reset_at: Option<u64>,
    allow_retry: bool,
) -> Option<OfficialWebSocketPlan> {
    if allow_retry {
        match runtime.hydrate_official_socket_handoff(plan, &request.body) {
            Ok(body) => request.body = body,
            Err(error) => {
                log::warn!("[QuotaPool] event=retry_declined stage=hydrate_before_selection reason={} chained={} request={}", error.category(), request.body.get("previous_response_id").is_some(), request.metadata.request_id);
                let _ = runtime
                    .prepare_websocket_quota_retry(plan, request, reset_at, false)
                    .await;
                return None;
            }
        }
    }
    let next = match runtime
        .prepare_websocket_quota_retry(plan, request, reset_at, allow_retry)
        .await
    {
        Ok(Some(next)) if allow_retry => next,
        Ok(_) => {
            log::info!("[QuotaPool] event=retry_declined stage=handoff reason=no_replacement allow_retry={allow_retry} request={}", request.metadata.request_id);
            return None;
        }
        Err(error) => {
            log::warn!(
                "[QuotaPool] event=retry_declined stage=prepare_replacement reason={} request={}",
                error.category(),
                request.metadata.request_id
            );
            return None;
        }
    };
    request.body = match runtime.hydrate_official_socket_handoff(&next, &request.body) {
        Ok(body) => body,
        Err(error) => {
            log::warn!(
                "[QuotaPool] event=retry_declined stage=hydrate_replacement reason={} request={}",
                error.category(),
                request.metadata.request_id
            );
            return None;
        }
    };
    request.body["type"] = json!("response.create");
    request.metadata.official_socket_affinity = None;
    Some(next)
}

/// Intercept quota failures before the first application frame reaches Codex.
/// Keep the same logical execution and queue its one retry ahead of new turns,
/// allowing any already-pipelined work on the old segment to drain first.
async fn prepare_quota_retry<S: ProxyRuntimeState>(
    state: &Arc<S>,
    headers: &HeaderMap,
    runtime: &ProxyRuntime,
    official: &mut Option<OfficialSegment>,
    connection_id: &str,
    message: Option<&Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>,
) -> Option<DeferredOfficialTurn> {
    let failed = failed_websocket_response(message?.as_ref().ok()?)?;
    if official_websocket_failure_category(&failed) != "provider_quota" {
        return None;
    }
    let Some(segment) = official.as_mut() else {
        log::info!("[QuotaPool] event=retry_declined stage=frame reason=no_official_segment connection={connection_id}");
        return None;
    };
    let Some(turn) = segment.pending.front_mut() else {
        log::info!("[QuotaPool] event=retry_declined stage=frame reason=no_pending_turn connection={connection_id}");
        return None;
    };
    let Some(plan) = turn.plan.as_mut() else {
        log::info!("[QuotaPool] event=retry_declined stage=frame reason=no_pending_plan connection={connection_id}");
        return None;
    };
    let allow_retry = !turn.quota_retried
        && turn.first_downstream_ms.is_none()
        && !turn.recorded
        && !turn.cancel_rx.as_ref().is_some_and(|rx| *rx.borrow());
    log::info!("[QuotaPool] event=retry_evaluate stage=frame allow_retry={allow_retry} chained={} connection={connection_id} request={}", turn.request.get("previous_response_id").is_some(), turn.request_id);
    if !allow_retry {
        log::info!("[QuotaPool] event=retry_declined stage=eligibility allow_retry=false quota_retried={} first_downstream={:?} recorded={} cancel={} connection={connection_id} request={}",
            turn.quota_retried, turn.first_downstream_ms, turn.recorded,
            turn.cancel_rx.as_ref().is_some_and(|rx| *rx.borrow()), turn.request_id);
    }
    let mut request = runtime_request_from_body(state, headers, &turn.request, connection_id);
    request.metadata = plan.request_metadata.clone();
    let next = prepare_quota_handoff(
        runtime,
        plan,
        &mut request,
        quota_failure_reset(&failed),
        allow_retry,
    )
    .await?;
    segment.quota_exhausted = true;
    log::info!("[QuotaPool] retrying pre-output Official quota failure on another socket connection={connection_id}");
    // A panic here aborts the whole Desktop process; decline instead.
    let Some(turn) = segment.pending.pop_front() else {
        log::warn!("[QuotaPool] event=retry_declined stage=frame reason=pending_turn_vanished connection={connection_id}");
        return None;
    };
    Some(DeferredOfficialTurn {
        plan: next,
        request: request.body.clone(),
        runtime_request: request,
        request_id: turn.request_id,
        execution_id: turn.execution_id,
        quota_retried: true,
    })
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
            if let Some(failed) = failed.as_ref() {
                if let Some(turn) = segment.pending.front_mut() {
                    if let Some(summary) = rejected_handoff_replay(turn, failed) {
                        let message = failed
                            .pointer("/response/error/message")
                            .or_else(|| failed.pointer("/error/message"))
                            .and_then(Value::as_str)
                            .unwrap_or("upstream Official WebSocket failure")
                            .to_owned();
                        log::warn!(
                            "[OfficialSocket] account handoff replay rejected; asking Codex for full input connection={connection_id} error={message:?} {summary}"
                        );
                        segment.stages.terminal_ms =
                            Some(segment.started.elapsed().as_millis() as u64);
                        record_official_pending_outcome(
                            runtime,
                            turn,
                            connection_id,
                            "protocol_failure",
                            Some("invalid_request"),
                            Some(&message),
                            &segment.stages,
                        );
                        let previous = turn
                            .request
                            .get("previous_response_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        segment.pending.pop_front();
                        // The same wrapped error Codex already handles for a
                        // missing local chain: it drops the incremental
                        // request and resends its own full input, which
                        // Official accepts on the new account.
                        if !send_runtime_failure(
                            client,
                            &RuntimeError::PreviousResponseNotFound(previous),
                        )
                        .await
                        {
                            return false;
                        }
                        *frames_out += 1;
                        return true;
                    }
                }
            }
            if application {
                if let Some(turn) = segment.pending.front_mut() {
                    if !turn.output_forwarded
                        && turn
                            .plan
                            .as_ref()
                            .is_some_and(|plan| plan.handoff_replay.is_some())
                    {
                        turn.output_forwarded = websocket_event(&message)
                            .is_none_or(|event| !handoff_preamble_event(&event));
                    }
                }
            }
            let closing = matches!(message, TungsteniteMessage::Close(_));
            if let TungsteniteMessage::Close(frame) = &message {
                log::warn!(
                    "[OfficialSocket] upstream sent Close connection={connection_id} {} pending_turns={} frames_out={} last_sent_frame_bytes={}",
                    describe_close_frame(frame.as_ref()),
                    segment.pending.len(),
                    *frames_out,
                    segment.last_sent_frame_bytes,
                );
            }
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
                segment.quota_exhausted = false;
                segment.last_input_tokens = response
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(segment.last_input_tokens);
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
                segment.quota_exhausted |= failed
                    .pointer("/response/error/code")
                    .or_else(|| failed.pointer("/error/code"))
                    .and_then(Value::as_str)
                    == Some("usage_limit_reached");
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
        None => {
            if let Some(segment) = official.as_ref() {
                log::warn!(
                    "[OfficialSocket] upstream stream ended without a Close frame connection={connection_id} pending_turns={} frames_out={} last_sent_frame_bytes={}",
                    segment.pending.len(),
                    *frames_out,
                    segment.last_sent_frame_bytes,
                );
            }
            false
        }
    }
}

fn describe_close_frame(
    frame: Option<&tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> String {
    match frame {
        Some(frame) => format!(
            "code={} reason={:?}",
            u16::from(frame.code),
            frame.reason.as_str()
        ),
        None => "code=none reason=none".to_string(),
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
    portable_warmup: &mut Option<PortableWarmup>,
    planned_request: Option<RuntimeRequest>,
    produced_response_id: &mut Option<String>,
) -> TurnOutcome {
    // The HTTP Responses body has no WebSocket event discriminator.
    if request.get("type").and_then(Value::as_str) == Some("response.create") {
        request
            .as_object_mut()
            .expect("response.create is a JSON object")
            .remove("type");
    }
    // `generate` is a WebSocket-only field: HTTP Responses rejects it with
    // `Unsupported parameter: generate`. A `generate=false` prewarm is
    // connection setup, not inference, so it is answered here and its input
    // is folded into the next turn that chains onto it.
    if let Some(generate) = request
        .as_object_mut()
        .and_then(|object| object.remove("generate"))
    {
        if generate == Value::Bool(false) {
            let (warmup, response) = portable_warmup_response(&request);
            *portable_warmup = Some(warmup);
            let turn_started = std::time::Instant::now();
            return match send_buffered_response_as_websocket_events(client, response, turn_started)
                .await
            {
                Ok(sent) => {
                    *frames_out += sent.frames_out;
                    TurnOutcome::Continue
                }
                Err(()) => TurnOutcome::Stop("client send failed".into()),
            };
        }
    }
    let mut runtime_request = planned_request
        .unwrap_or_else(|| runtime_request_from_body(state, headers, &request, connection_id));
    runtime_request.body = request.clone();
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
                    *produced_response_id = sent.response_id.clone();
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
                    *produced_response_id = outcome.response_id.clone();
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
            if matches!(error, RuntimeError::PreviousResponseNotFound(_)) {
                TurnOutcome::Continue
            } else {
                TurnOutcome::Stop(error.to_string())
            }
        }
    }
}

/// A `generate=false` prewarm answered locally on the portable path. Codex
/// chains its next `response.create` onto this id and sends only the input
/// added since, so the prewarm input must be restored before that turn runs.
#[derive(Debug)]
struct PortableWarmup {
    response_id: String,
    input: Value,
}

fn portable_warmup_response(request: &Value) -> (PortableWarmup, Value) {
    let response_id = format!("resp_vellum_warmup_{}", ulid::Ulid::new());
    let response = json!({
        "id": response_id,
        "object": "response",
        "status": "completed",
        "model": request.get("model").cloned().unwrap_or(Value::Null),
        "output": [],
    });
    let warmup = PortableWarmup {
        response_id,
        input: request.get("input").cloned().unwrap_or_else(|| json!([])),
    };
    (warmup, response)
}

/// Rewrite a turn chained onto a locally answered prewarm into a standalone
/// request: the prewarm id exists nowhere upstream or in history. Any other
/// turn discards the pending prewarm, since Codex only chains the next one.
fn resume_portable_warmup(request: &mut Value, warmup: &mut Option<PortableWarmup>) {
    let Some(warmup) = warmup.take() else {
        return;
    };
    let Some(object) = request.as_object_mut() else {
        return;
    };
    if object.get("previous_response_id").and_then(Value::as_str)
        != Some(warmup.response_id.as_str())
    {
        return;
    }
    let Value::Array(mut input) = warmup.input else {
        return;
    };
    match object.get("input") {
        Some(Value::Array(delta)) => input.extend(delta.iter().cloned()),
        None => {}
        Some(_) => return,
    }
    object.remove("previous_response_id");
    object.insert("input".into(), Value::Array(input));
}

fn request_for_history(mut value: Value) -> Value {
    if value.get("type").and_then(Value::as_str) == Some("response.create") {
        if let Some(object) = value.as_object_mut() {
            object.remove("type");
        }
    }
    value
}

/// The `response.id` carried by a `response.created`/`response.completed`
/// SSE block, if any.
fn response_id_from_sse_block(block: &str) -> Option<String> {
    block
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find_map(|event| {
            event
                .pointer("/response/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
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

/// Lifecycle frames that precede any model output. A handoff replay that
/// fails after only these is retried without Codex having seen output.
fn handoff_preamble_event(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("response.created" | "response.in_progress" | "response.queued")
    )
}

/// A locally rebuilt account-handoff frame that Official rejected as an
/// invalid request before any output. Returns the replay's diagnostic
/// summary; Codex's own full-input retry is the recovery.
fn rejected_handoff_replay<'a>(turn: &'a OfficialPendingTurn, failed: &Value) -> Option<&'a str> {
    if turn.output_forwarded || turn.recorded {
        return None;
    }
    let summary = turn.plan.as_ref()?.handoff_replay.as_deref()?;
    (official_websocket_failure_category(failed) == "invalid_request").then_some(summary)
}

/// Redacted shape of a locally rebuilt handoff frame against Codex's
/// original: top-level keys, which control fields changed, and the declared
/// tool names. Never includes item content, schemas or instruction text.
fn handoff_replay_summary(original: &Value, replay: &Value) -> String {
    fn tool_names(tools: Option<&Value>) -> String {
        tools
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| {
                        let kind = tool.get("type").and_then(Value::as_str).unwrap_or("?");
                        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
                        match tool.get("tools") {
                            Some(children) => {
                                format!("{kind}:{name}{{{}}}", tool_names(Some(children)))
                            }
                            None => format!("{kind}:{name}"),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    }
    let keys = replay
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|key| *key != "input")
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let changed = [
        "previous_response_id",
        "instructions",
        "include",
        "store",
        "tools",
        "tool_choice",
        "model",
    ]
    .into_iter()
    .filter(|field| original.get(*field) != replay.get(*field))
    .collect::<Vec<_>>()
    .join(",");
    let input = crate::request::request_input_items(replay);
    let declarations = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .map(|item| {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.split_once('_'))
                .map_or("none", |(prefix, _)| prefix);
            format!("id={id}:[{}]", tool_names(item.get("tools")))
        })
        .collect::<Vec<_>>()
        .join(";");
    let without_id = input
        .iter()
        .filter(|item| item.get("id").and_then(Value::as_str).is_none())
        .count();
    format!(
        "keys=[{keys}] changed=[{changed}] tools=[{}] additional_tools=[{declarations}] input_items={} items_without_id={without_id}",
        tool_names(replay.get("tools")),
        input.len()
    )
}

fn official_websocket_failure_category(event: &Value) -> &str {
    let error = event
        .pointer("/response/error")
        .or_else(|| event.get("error"));
    if event.get("status").and_then(Value::as_u64) == Some(429)
        || error
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .is_some_and(|code| {
                matches!(
                    code,
                    "usage_limit_reached" | "usage_limit_exceeded" | "insufficient_quota"
                )
            })
    {
        return "provider_quota";
    }
    if error.is_some_and(crate::error::is_upstream_server_error) {
        return "provider_unavailable";
    }
    // Codex recognizes this upstream code and reconnects. Preserve the frame
    // verbatim, but record the expired transport as a retryable availability
    // failure rather than a fatal prompt error.
    if error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some("websocket_connection_limit_reached")
    {
        return "provider_unavailable";
    }
    if let Some(category) = event
        .pointer("/response/error/category")
        .or_else(|| event.pointer("/error/category"))
        .and_then(Value::as_str)
    {
        return category;
    }
    match error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
    {
        Some("invalid_request_error") => "invalid_request",
        Some("authentication_error") => "provider_auth",
        // Codex maps the verbatim usage_limit_reached frame to its own
        // usage-limit error; Vellum only has to account for it as quota.
        Some("insufficient_quota") | Some("rate_limit_error") | Some("usage_limit_reached") => {
            "provider_quota"
        }
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
) -> Result<OfficialUpstreamSocket, Box<tokio_tungstenite::tungstenite::Error>> {
    connect_async_with_config(request, None, true)
        .await
        .map(|(socket, _)| socket)
        .map_err(Box::new)
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
///
/// `session-id` is the backend's prompt-cache affinity key: without it a full
/// `response.create` on a fresh socket lands on a cold cache shard even when
/// the body (and `prompt_cache_key`) is byte-identical to the previous turn.
fn should_forward_official_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "user-agent"
        || name == "openai-beta"
        || name == "session-id"
        || name == "session_id"
        || name == "thread-id"
        || name == "x-client-request-id"
        || name == "originator"
        || name == "traceparent"
        || name == "tracestate"
        || name == "baggage"
        || name == "x-request-id"
        || name.starts_with("x-codex-")
        || name.starts_with("x-openai-")
}

/// `response.create.model` is rewritten because catalog IDs are local aliases.
/// The shared Official compatibility guard omits the Codex-local
/// `reasoning.summary = "none"` sentinel. Every other application and control
/// field is forwarded unchanged.
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
    value.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidRequest("response.create must be a JSON object".into())
    })?;
    let value = crate::adapter::prepare_openai_official_native(&value, upstream_model);
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
        400 if diagnostic.to_ascii_lowercase().contains("quota") => {
            RuntimeError::provider_quota(message, None, true)
        }
        408 | 409 | 425 | 426 => RuntimeError::ProviderProtocol(message),
        _ if (400..500).contains(&status) => RuntimeError::ProviderRequestRejected {
            upstream_status: status,
            message,
        },
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

/// A WebSocket response terminal is only a model invocation boundary. Tool
/// results continue the same Codex root turn, which may span many invocations.
/// Retain trusted socket identity when an incremental frame omits it; never
/// inherit across an explicit thread change or a conflicting identity.
fn apply_socket_affinity(
    request: &mut RuntimeRequest,
    previous: Option<&crate::request::RequestMetadata>,
    account_hash: Option<&str>,
    input_tokens: u64,
    quota_exhausted: bool,
) {
    let Some(previous) = previous else { return };
    if request.metadata.codex_identity_conflicted() || previous.codex_identity_conflicted() {
        return;
    }
    if let (Some(current), Some(prior)) =
        (&request.metadata.codex_identity, &previous.codex_identity)
    {
        if current
            .thread_id
            .as_ref()
            .zip(prior.thread_id.as_ref())
            .is_some_and(|(a, b)| a != b)
            || current
                .session_id
                .as_ref()
                .zip(prior.session_id.as_ref())
                .is_some_and(|(a, b)| a != b)
        {
            return;
        }
    }
    let roots_present = request
        .metadata
        .codex_identity
        .as_ref()
        .is_some_and(|id| id.root_turn_id.is_some())
        || previous
            .codex_identity
            .as_ref()
            .is_some_and(|id| id.root_turn_id.is_some());
    let current_turn = request.metadata.codex_identity.as_ref().and_then(|id| {
        if roots_present {
            id.root_turn_id.as_ref()
        } else {
            id.turn_id.as_ref()
        }
    });
    let prior_turn = previous.codex_identity.as_ref().and_then(|id| {
        if roots_present {
            id.root_turn_id.as_ref()
        } else {
            id.turn_id.as_ref()
        }
    });
    let turn_in_progress = match (current_turn, prior_turn) {
        (Some(current), Some(prior)) => current == prior,
        _ => request
            .body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some(),
    };
    if let Some(prior) = &previous.codex_identity {
        let current = request
            .metadata
            .codex_identity
            .get_or_insert_with(|| prior.clone());
        current.session_id = current
            .session_id
            .clone()
            .or_else(|| prior.session_id.clone());
        current.thread_id = current
            .thread_id
            .clone()
            .or_else(|| prior.thread_id.clone());
        if turn_in_progress {
            current.root_turn_id = current
                .root_turn_id
                .clone()
                .or_else(|| prior.root_turn_id.clone());
            current.context_window_id = current
                .context_window_id
                .clone()
                .or_else(|| prior.context_window_id.clone());
        }
        if current.session_id == prior.session_id && current.thread_id == prior.thread_id {
            current.trust = prior.trust;
        }
    }
    let prior = previous.official_socket_affinity.as_ref();
    let compressed = request
        .metadata
        .codex_identity
        .as_ref()
        .and_then(|id| id.context_window_id.as_ref())
        .zip(
            previous
                .codex_identity
                .as_ref()
                .and_then(|id| id.context_window_id.as_ref()),
        )
        .is_some_and(|(a, b)| a != b);
    let live_account = account_hash.is_some();
    if let Some(account_hash) = account_hash.or_else(|| prior.map(|a| a.account_hash.as_str())) {
        let account_hash = account_hash.strip_prefix("sha256:").unwrap_or(account_hash);
        let same_account = prior.is_some_and(|a| a.account_hash == account_hash);
        request.metadata.official_socket_affinity = Some(crate::request::OfficialSocketAffinity {
            account_hash: account_hash.to_owned(),
            turn_in_progress,
            input_tokens: if compressed {
                0
            } else if input_tokens > 0 {
                input_tokens
            } else {
                prior.filter(|_| same_account).map_or(0, |a| a.input_tokens)
            },
            quota_exhausted: if live_account {
                quota_exhausted
            } else {
                prior.is_some_and(|a| same_account && a.quota_exhausted)
            },
        });
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
    response_id: Option<String>,
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
    /// A model-output frame of this turn already reached Codex.
    output_forwarded: bool,
    cancelled_at: Option<std::time::Instant>,
    quota_retried: bool,
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
            output_forwarded: false,
            cancelled_at: None,
            quota_retried: false,
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
    let status = match (outcome, error_category) {
        ("client_cancel" | "client_disconnect", _) => 499,
        ("protocol_failure", Some("invalid_request")) => 400,
        (_, Some("provider_unavailable")) => 503,
        (_, Some("provider_quota")) => 429,
        _ => 502,
    };
    runtime.finish_terminal_once_with_record(
        &turn.execution_id,
        outcome,
        UsageRecord {
            route_id: plan.route_id.clone(),
            provider: plan.provider_name.clone(),
            model: plan.upstream_model.clone(),
            status,
            request_id: Some(turn.request_id.clone()),
            connection_id: Some(connection_id.to_string()),
            duration_ms: turn.started.elapsed().as_millis() as u64,
            first_byte_ms: turn.first_byte_ms,
            first_event_ms: turn.first_byte_ms,
            first_downstream_frame_ms: turn.first_downstream_ms,
            outcome: Some(outcome.to_string()),
            error_category: error_category.map(str::to_string),
            error: error.map(str::to_string),
            stage_times_ms: Some(stages.for_turn(turn)),
            control_account_hash: plan.control_account_hash.clone(),
            execution_account_hash: plan.execution_account_hash.clone(),
            selection_revision: plan.selection_revision,
            upstream_attempted: Some(true),
            ..Default::default()
        },
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
            if let Some(id) = response_id_from_sse_block(&block) {
                outcome.response_id = Some(id);
            }
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
            if let Some(id) = response_id_from_sse_block(&block) {
                outcome.response_id = Some(id);
            }
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
    let mut outcome = StreamForwardOutcome {
        response_id: response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..StreamForwardOutcome::default()
    };
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

/// Codex parses `response.failed.error` with a string `code`; a numeric one
/// fails that parse, so the turn surfaced only "response.failed event
/// received" and Vellum's message was lost. Codex then retries every
/// failure, so a request Vellum rejects as malformed (HTTP 400 on the HTTP
/// transport, which Codex treats as fatal) carries `invalid_prompt`, the
/// code Codex maps to the same non-retryable invalid-request error. The HTTP
/// status stays available as `status`.
fn runtime_failure_event(error: &RuntimeError) -> Value {
    if matches!(error, RuntimeError::PreviousResponseNotFound(_)) {
        // Codex recognizes this wrapped WS error before parsing Responses
        // events, discards its incremental request and retries full input.
        return json!({
            "type": "error", "status": 400,
            "error": {
                "type": "invalid_request_error", "code": "previous_response_not_found",
                "message": error.to_string()
            }
        });
    }
    error.to_response_failed_event()
}

async fn send_runtime_failure(client: &mut WebSocket, error: &RuntimeError) -> bool {
    let value = runtime_failure_event(error);
    client
        .send(AxumWsMessage::Text(value.to_string().into()))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_request(thread: Option<&str>, root: Option<&str>) -> RuntimeRequest {
        use crate::codex_metadata::{CodexIdentityTrust, CodexOpaqueId, CodexTurnIdentity};
        RuntimeRequest {
            body: json!({"model":"vellum-official", "previous_response_id":"resp_previous", "input":[{"type":"function_call_output","call_id":"call_shell","output":"done"}]}),
            endpoint: RuntimeEndpoint::Responses,
            incoming_auth: Default::default(),
            execution_environment: crate::environment::ExecutionEnvironment::posix_reference(),
            metadata: crate::request::RequestMetadata {
                connection_id: Some("socket-one".into()),
                codex_identity: thread.or(root).map(|_| CodexTurnIdentity {
                    session_id: thread
                        .map(|_| CodexOpaqueId::new("session_id", "session-one").unwrap()),
                    thread_id: thread
                        .map(|thread| CodexOpaqueId::new("thread_id", thread).unwrap()),
                    root_turn_id: root
                        .map(|root| CodexOpaqueId::new("root_turn_id", root).unwrap()),
                    trust: CodexIdentityTrust::Structured,
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn incremental_socket_keeps_thread_root_and_cumulative_context() {
        let first = socket_request(Some("thread-one"), Some("root-one"));
        let mut next = socket_request(None, Some("root-one"));
        apply_socket_affinity(
            &mut next,
            Some(&first.metadata),
            Some("sha256:account-a"),
            300_000,
            false,
        );
        assert_eq!(
            next.metadata.codex_thread_key(),
            first.metadata.codex_thread_key()
        );
        let affinity = next.metadata.official_socket_affinity.as_ref().unwrap();
        assert!(affinity.turn_in_progress);
        assert_eq!(affinity.input_tokens, 300_000);
        assert_eq!(affinity.account_hash, "account-a");
        assert_eq!(
            crate::history::resolve_conversation_key(&next, None).key,
            "codex:session-one:thread-one"
        );
        let route = official_route_config("https://example.test".into()).to_route();
        assert_eq!(
            crate::official_auth::OfficialRoutingContext::from_request(&route, &next)
                .estimated_input_tokens,
            300_000
        );

        let previous = next.metadata.clone();
        let mut boundary = socket_request(None, Some("root-two"));
        apply_socket_affinity(
            &mut boundary,
            Some(&previous),
            Some("account-a"),
            300_000,
            true,
        );
        assert!(
            !boundary
                .metadata
                .official_socket_affinity
                .as_ref()
                .unwrap()
                .turn_in_progress
        );
        assert!(
            boundary
                .metadata
                .official_socket_affinity
                .as_ref()
                .unwrap()
                .quota_exhausted
        );
        assert_eq!(
            boundary.metadata.codex_thread_key(),
            first.metadata.codex_thread_key()
        );
    }

    #[test]
    fn socket_affinity_does_not_cross_threads_or_conflicted_identity() {
        let first = socket_request(Some("thread-one"), Some("root-one"));
        for conflict in [false, true] {
            let mut next = socket_request(Some("thread-two"), Some("root-one"));
            if conflict {
                next.metadata.codex_identity.as_mut().unwrap().trust =
                    crate::codex_metadata::CodexIdentityTrust::Conflict;
            }
            apply_socket_affinity(
                &mut next,
                Some(&first.metadata),
                Some("account-a"),
                300_000,
                false,
            );
            assert!(next.metadata.official_socket_affinity.is_none());
            assert_eq!(
                next.metadata
                    .codex_identity
                    .as_ref()
                    .unwrap()
                    .thread_id
                    .as_deref(),
                Some("thread-two")
            );
        }
    }

    #[test]
    fn socket_context_can_shrink_after_compaction_and_usage_limit_is_not_permanent() {
        let mut first = socket_request(Some("thread-one"), Some("root-one"));
        first.metadata.official_socket_affinity = Some(crate::request::OfficialSocketAffinity {
            account_hash: "account-a".into(),
            turn_in_progress: true,
            input_tokens: 300_000,
            quota_exhausted: true,
        });
        let mut next = socket_request(None, Some("root-one"));
        apply_socket_affinity(
            &mut next,
            Some(&first.metadata),
            Some("account-a"),
            20_000,
            false,
        );
        let affinity = next.metadata.official_socket_affinity.unwrap();
        assert_eq!(
            affinity.input_tokens, 20_000,
            "new actual usage supersedes the old large context"
        );
        assert!(
            !affinity.quota_exhausted,
            "a successful native response clears its stale quota rejection"
        );
    }
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
    fn runtime_failure_code_is_a_string_and_malformed_requests_are_final() {
        let invalid = runtime_failure_event(&RuntimeError::InvalidRequest("bad shape".into()));
        let error = &invalid["response"]["error"];
        assert_eq!(error["code"], "invalid_prompt");
        assert_eq!(error["status"], 400);
        assert_eq!(error["category"], "invalid_request");
        assert!(error["message"].as_str().unwrap().contains("bad shape"));

        let unauthorized =
            runtime_failure_event(&RuntimeError::ProviderUnauthorized("free tier".into()));
        let error = &unauthorized["response"]["error"];
        assert_eq!(error["code"], "provider_unauthorized");
        assert_eq!(error["status"], 401);
        assert!(error["code"].is_string());
    }

    #[test]
    fn official_handshake_rejections_keep_fatal_and_retryable_codes() {
        for (status, message, code) in [
            (400, "Unsupported parameter", "invalid_prompt"),
            (422, "Invalid input", "invalid_prompt"),
            (400, "quota exhausted", "provider_quota"),
            (401, "denied", "provider_unauthorized"),
            (403, "denied", "provider_unauthorized"),
            (429, "limited", "provider_quota"),
            (408, "try later", "provider_protocol"),
            (409, "try later", "provider_protocol"),
            (425, "try later", "provider_protocol"),
            (426, "upgrade", "provider_protocol"),
            (503, "unavailable", "provider_unavailable"),
        ] {
            let response = tokio_tungstenite::tungstenite::http::Response::builder()
                .status(status)
                .body(Some(message.as_bytes().to_vec()))
                .unwrap();
            let event = runtime_failure_event(&websocket_handshake_error(response));
            assert_eq!(event["response"]["error"]["code"], code);
            assert!(event["response"]["error"]["code"].is_string());
        }
    }

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
            output_forwarded: false,
            cancelled_at: None,
            quota_retried: false,
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
            output_forwarded: false,
            cancelled_at: None,
            quota_retried: false,
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
    fn official_response_create_changes_only_model_and_omits_disabled_summary() {
        let original = br#"{"type":"response.create","model":"catalog-alias","reasoning":{"effort":"high","summary":"none"},"input":[{"type":"reasoning","encrypted_content":"opaque"}],"stream":true}"#;
        let prepared = prepare_response_create_bytes(original, "gpt-upstream")
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&prepared).unwrap();
        assert_eq!(value["type"], "response.create");
        assert_eq!(value["model"], "gpt-upstream");
        assert_eq!(value["reasoning"]["effort"], "high");
        assert!(value["reasoning"].get("summary").is_none());
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
        assert!(should_forward_official_header("session-id"));
        assert!(should_forward_official_header("session_id"));
        assert!(should_forward_official_header("thread-id"));
        assert!(should_forward_official_header("x-client-request-id"));
        assert!(should_forward_official_header("originator"));
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
            .insert("session-id", "native-session".parse().unwrap());
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
            assert_eq!(headers["session-id"], "native-session");
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
        run_official_websocket_account_switch(false, "switch").await;
    }

    #[tokio::test]
    async fn official_reconnect_after_account_switch_replays_history_on_socket() {
        run_official_websocket_account_switch(false, "switch_reconnect").await;
    }

    #[tokio::test]
    async fn rejected_account_handoff_replay_asks_codex_for_full_input() {
        run_official_websocket_account_switch(false, "switch_rejected").await;
    }

    #[tokio::test]
    async fn official_websocket_switches_users_in_the_same_workspace() {
        run_official_websocket_account_switch(true, "switch").await;
    }

    #[tokio::test]
    async fn official_websocket_bridge_replays_same_account_history() {
        run_official_websocket_account_switch(false, "bridge").await;
    }

    #[tokio::test]
    async fn official_websocket_switch_missing_history_requests_full_input_retry() {
        run_official_websocket_account_switch(false, "missing").await;
    }

    #[tokio::test]
    async fn official_websocket_selection_revision_change_replays_history() {
        run_official_websocket_account_switch(false, "revision").await;
    }

    #[tokio::test]
    async fn official_same_account_revision_during_root_turn_keeps_native_socket() {
        run_official_websocket_account_switch(false, "revision_active").await;
    }

    #[tokio::test]
    async fn official_reconnect_restores_thread_root_and_context_before_routing() {
        run_official_websocket_account_switch(false, "revision_active_reconnect").await;
    }

    #[tokio::test]
    async fn official_websocket_streaming_bridge_replays_history_and_records_account() {
        run_official_websocket_account_switch(false, "bridge_stream").await;
    }

    async fn run_official_websocket_account_switch(shared_workspace: bool, mode: &str) {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        fn desktop_tools() -> Value {
            json!([
                serde_json::from_str::<Value>(include_str!("../tests/fixtures/code-mode-exec.json")).unwrap(),
                {"type":"namespace","name":"collaboration","description":"Coordinate agents", "tools":[
                    {"type":"function","name":"followup_task","description":"Continue an agent task", "parameters":{
                        "type":"object", "properties":{"target":{"type":"string"},"message":{"type":"string"}},
                        "required":["target","message"],"additionalProperties":false}},
                    {"type":"function","name":"send_message","parameters":{"type":"object", "properties":{"target":{"type":"string"},"message":{"type":"string"}},"required":["target","message"]}}
                ]}
            ])
        }

        fn account_hash(account_id: &str) -> String {
            format!("{:x}", Sha256::digest(account_id.as_bytes()))
        }

        fn jwt_for(account_id: &str, shared_workspace: bool) -> String {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                if shared_workspace {
                    json!({"chatgpt_account_id": "shared-workspace", "sub": account_id})
                } else {
                    json!({"chatgpt_account_id": account_id})
                }
                .to_string(),
            );
            format!("header.{payload}.signature")
        }

        fn execution_from_headers(headers: &HeaderMap) -> String {
            let token = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));
            token
                .and_then(crate::chatgpt_identity_from_jwt)
                .map(|id| id.credential_id)
                .unwrap_or_else(|| {
                    headers
                        .get("chatgpt-account-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                })
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
        let identity_for = |account: &str| {
            crate::chatgpt_identity_from_jwt(&jwt_for(account, shared_workspace))
                .map_or_else(|| account.to_string(), |id| id.credential_id)
        };
        let identity_a = identity_for("acct-a");
        let identity_b = identity_for("acct-b");
        for account_id in ["acct-a", "acct-b"] {
            let path = auth_root
                .join("grants")
                .join(format!("{}.json", account_hash(&identity_for(account_id))));
            crate::official_auth::write_grant_atomic(
                &path,
                &crate::official_auth::FileOfficialGrant {
                    credential_id: shared_workspace.then(|| identity_for(account_id)),
                    account_id: if shared_workspace {
                        "shared-workspace"
                    } else {
                        account_id
                    }
                    .into(),
                    access_token: jwt_for(account_id, shared_workspace),
                    refresh_token: format!("refresh-{account_id}"),
                    expires_at_ms: chrono::Utc::now().timestamp_millis() + 600_000,
                },
            )
            .unwrap();
        }
        select_account(&auth_root, &identity_a, 1);

        #[derive(Clone, Default)]
        struct Capture {
            websocket_accounts: Arc<Mutex<Vec<String>>>,
            websocket_frames: Arc<Mutex<Vec<(String, Value)>>>,
            http_requests: Arc<Mutex<Vec<(String, Value)>>>,
            http_headers: Arc<Mutex<Vec<HeaderMap>>>,
            responses: Arc<AtomicUsize>,
        }

        let capture = Capture::default();
        let get_capture = capture.clone();
        let post_capture = capture.clone();
        let check_contract = mode != "missing";
        let reject_replay = mode == "switch_rejected";
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
                let capture = get_capture.clone();
                async move {
                    assert_eq!(headers["session-id"], "bridge-affinity");
                    assert_eq!(headers["session_id"], "bridge-affinity-legacy");
                    let account = execution_from_headers(&headers);
                    capture.websocket_accounts.lock().unwrap().push(account.clone());
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            let request: Value = serde_json::from_str(&text).unwrap();
                            if request.get("type").and_then(Value::as_str)
                                == Some("response.create")
                            {
                                assert_eq!(request["tools"], desktop_tools(), "native WS contract");
                                capture
                                    .websocket_frames
                                    .lock()
                                    .unwrap()
                                    .push((account.clone(), request.clone()));
                                let turn_count = capture.responses.fetch_add(1, Ordering::SeqCst) + 1;
                                if reject_replay && turn_count == 2 {
                                    // What Official answered the locally
                                    // rebuilt handoff on 2026-10-17.
                                    socket
                                        .send(AxumWsMessage::Text(
                                            json!({"type":"error","status":400,"error":{
                                                "type":"invalid_request_error","param":"tools","code":null,
                                                "message":"Invalid Value: 'tools'. Function 'collaboration.followup_task' is reserved for use by this model and must match the configured schema."}})
                                            .to_string()
                                            .into(),
                                        ))
                                        .await
                                        .unwrap();
                                    continue;
                                }
                                socket
                                    .send(AxumWsMessage::Text(
                                        json!({
                                            "type": "response.completed",
                                            "response": {
                                                "id": if turn_count == 1 { "resp_account_a" } else { "resp_account_next" },
                                                "object": "response",
                                                "status": "completed",
                                                "usage":{"input_tokens":300_000,"output_tokens":10,"input_tokens_details":{"cached_tokens":297_000}},
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
                        if check_contract {
                            assert_eq!(
                                body["tools"],
                                desktop_tools(),
                                "HTTP resume must not flatten or replace collaboration tools"
                            );
                        }
                        capture.http_headers.lock().unwrap().push(headers.clone());
                        let account = execution_from_headers(&headers);
                        let streaming = body["stream"] == true;
                        capture.http_requests.lock().unwrap().push((account, body));
                        let response = json!({
                            "id": "resp_account_b",
                            "object": "response",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "answer from B"}]
                            }]
                        });
                        if streaming {
                            axum::response::IntoResponse::into_response((
                                [("content-type", "text/event-stream")],
                                format!(
                                    "data: {}\n\n",
                                    json!({"type": "response.completed", "response": response})
                                ),
                            ))
                        } else {
                            axum::response::IntoResponse::into_response(axum::Json(response))
                        }
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

        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let mut handshake = format!("ws://{proxy_address}/v1/responses")
            .into_client_request()
            .unwrap();
        handshake
            .headers_mut()
            .insert("session-id", "bridge-affinity".parse().unwrap());
        handshake
            .headers_mut()
            .insert("session_id", "bridge-affinity-legacy".parse().unwrap());
        let (mut client, _) = tokio_tungstenite::connect_async(handshake.clone())
            .await
            .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "input": [{"role": "user", "content": "first question"}],
                    "tools": desktop_tools(),
                    "client_metadata":{"x-codex-turn-metadata":{"session_id":"switch-session","thread_id":"switch-thread","root_turn_id":"root-one"}},
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

        if mode == "revision_active_reconnect" || mode == "switch_reconnect" {
            let _ = client.close(None).await;
            let (replacement, _) = tokio_tungstenite::connect_async(handshake).await.unwrap();
            client = replacement;
        }

        if !mode.starts_with("bridge") {
            select_account(
                &auth_root,
                if mode.starts_with("revision") {
                    &identity_a
                } else {
                    &identity_b
                },
                2,
            );
        }
        let tools = desktop_tools();
        // An account or selection change on a live Official socket replays
        // the history on a socket; only Auto Review keeps the HTTP bridge.
        let socket_handoff = matches!(
            mode,
            "switch" | "switch_reconnect" | "switch_rejected" | "revision"
        );
        client
            .send(TungsteniteMessage::Text(
                json!({
                    "type": "response.create",
                    "model": "vellum-official",
                    "tools": tools, "tool_choice":"auto", "parallel_tool_calls":true,
                    "client_metadata":{"x-codex-turn-metadata":{"root_turn_id":if mode.starts_with("revision_active") {"root-one"} else {"root-two"}}},
                    "include":["reasoning.encrypted_content"],
                    "text":{"format":{"type":"text"}},
                    "reasoning":{"effort":"high","summary":"auto"},
                    "prompt_cache_key":"account-switch-contract",
                    "previous_response_id": if mode == "missing" { "resp_not_in_history" } else { "resp_account_a" },
                    "instructions": if mode.starts_with("bridge") {
                        "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy"
                    } else { "be brief" },
                    "input": [{"role": "user", "content": "follow-up question"}],
                    "stream": mode == "bridge_stream"
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
                if reject_replay && event["type"] == "error" {
                    // Codex's handling of this code: drop the incremental
                    // request and resend its own full input, unchained.
                    assert_eq!(
                        event["error"]["code"], "previous_response_not_found",
                        "{event}"
                    );
                    assert!(
                        !text.contains("reserved"),
                        "the rejection is not surfaced: {event}"
                    );
                    client.send(TungsteniteMessage::Text(json!({
                        "type": "response.create",
                        "model": "vellum-official",
                        "tools": tools, "tool_choice":"auto", "parallel_tool_calls":true,
                        "client_metadata":{"x-codex-turn-metadata":{"root_turn_id":"root-two"}},
                        "include":["reasoning.encrypted_content"],
                        "text":{"format":{"type":"text"}},
                        "reasoning":{"effort":"high","summary":"auto"},
                        "prompt_cache_key":"account-switch-contract",
                        "instructions": "be brief",
                        "input": [{"role": "user", "content": "first question"},
                            {"role": "assistant", "content": "answer from A"},
                            {"role": "user", "content": "follow-up question"}],
                        "stream": false
                    }).to_string().into())).await.unwrap();
                    continue;
                }
                if event["type"] == "error" || event["type"] == "response.failed" {
                    assert_eq!(mode, "missing", "unexpected failure: {event}");
                    assert_eq!(event["error"]["code"], "previous_response_not_found");
                    assert!(capture.http_requests.lock().unwrap().is_empty());
                    let rows = runtime.usage_records().unwrap();
                    let row = rows.last().unwrap();
                    assert_eq!(row.upstream_attempted, Some(false));
                    assert_eq!(
                        row.execution_account_hash.as_deref(),
                        Some(format!("sha256:{}", account_hash(&identity_b)).as_str())
                    );
                    client.send(TungsteniteMessage::Text(json!({
                        "type": "response.create", "model": "vellum-official",
                        "instructions": "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy",
                        "input": [{"role": "user", "content": "first question"},
                            {"role": "assistant", "content": "answer from A"},
                            {"role": "user", "content": "follow-up question"}],
                        "stream": false
                    }).to_string().into())).await.unwrap();
                    continue;
                }
                if event["type"] == "response.completed" {
                    assert_eq!(
                        event["response"]["id"],
                        if mode.starts_with("revision_active") || socket_handoff {
                            "resp_account_next"
                        } else {
                            "resp_account_b"
                        },
                        "websocket accounts={:?}, http requests={:?}",
                        capture.websocket_accounts.lock().unwrap(),
                        capture.http_requests.lock().unwrap()
                    );
                    break;
                }
            }
        }

        let websocket_accounts = capture.websocket_accounts.lock().unwrap().clone();
        let requests = capture.http_requests.lock().unwrap().clone();
        if socket_handoff {
            let expected = if mode.starts_with("switch") {
                &identity_b
            } else {
                &identity_a
            };
            assert!(
                requests.is_empty(),
                "handoff must not use HTTP: {requests:?}"
            );
            assert_eq!(websocket_accounts.last(), Some(expected));
            let frames = capture.websocket_frames.lock().unwrap().clone();
            assert_eq!(frames.len(), if reject_replay { 3 } else { 2 });
            if reject_replay {
                let (account, retry) = &frames[2];
                assert_eq!(account, expected);
                assert!(retry.get("previous_response_id").is_none());
                assert_eq!(
                    retry["input"].as_array().map(Vec::len),
                    Some(3),
                    "Codex's full input reaches upstream unchanged: {retry}"
                );
            }
            let (account, body) = &frames[1];
            assert_eq!(account, expected);
            assert!(
                body.get("previous_response_id").is_none(),
                "old response id must not reach the replacement socket: {body}"
            );
            // Only conversation items may change; every request control and
            // the tool manifest stay exactly what Codex sent.
            let mut original = json!({
                "type": "response.create",
                "model": "vellum-official",
                "tools": tools, "tool_choice":"auto", "parallel_tool_calls":true,
                "client_metadata":{"x-codex-turn-metadata":{"root_turn_id":"root-two"}},
                "include":["reasoning.encrypted_content"],
                "text":{"format":{"type":"text"}},
                "reasoning":{"effort":"high","summary":"auto"},
                "prompt_cache_key":"account-switch-contract",
                "instructions": "be brief",
                "stream": false
            });
            let mut sent = body.clone();
            sent.as_object_mut().unwrap().remove("input");
            original["model"] = sent["model"].clone();
            assert_eq!(sent, original);
            let input_text = body["input"].to_string();
            assert!(input_text.contains("first question"), "{input_text}");
            assert!(input_text.contains("answer from A"), "{input_text}");
            assert!(input_text.contains("follow-up question"), "{input_text}");
            let rows = runtime.usage_records().unwrap();
            let row = rows.last().unwrap();
            assert_eq!(row.status, 200);
            assert_eq!(
                row.conversation_identity.as_deref(),
                Some("codex:switch-session:switch-thread")
            );
            assert_eq!(
                row.execution_account_hash.as_deref(),
                Some(format!("sha256:{}", account_hash(expected)).as_str())
            );
            let _ = client.close(None).await;
            proxy_task.abort();
            upstream_task.abort();
            return;
        }
        assert_eq!(
            websocket_accounts,
            if mode == "revision_active_reconnect" {
                vec![identity_a.clone(), identity_a.clone()]
            } else {
                vec![identity_a.clone()]
            }
        );
        if mode.starts_with("revision_active") {
            assert!(
                requests.is_empty(),
                "same-account revision must not discard the running root's native continuation"
            );
            let rows = runtime.usage_records().unwrap();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.status == 200
                && row.conversation_identity.as_deref()
                    == Some("codex:switch-session:switch-thread")));
            let _ = client.close(None).await;
            proxy_task.abort();
            upstream_task.abort();
            return;
        }
        for headers in capture.http_headers.lock().unwrap().iter() {
            assert_eq!(headers["session-id"], "bridge-affinity");
            assert_eq!(headers["session_id"], "bridge-affinity-legacy");
        }
        assert_eq!(
            requests.len(),
            1,
            "account handoff must use one portable POST"
        );
        assert_eq!(
            requests[0].0,
            if mode.starts_with("bridge") || mode == "revision" {
                identity_a.clone()
            } else {
                identity_b.clone()
            }
        );
        assert!(
            requests[0].1.get("previous_response_id").is_none(),
            "old account response id must not reach the new account: {}",
            requests[0].1
        );
        if mode != "missing" {
            let body = &requests[0].1;
            assert_eq!(body["tools"], tools, "mode={mode}");
            assert_eq!(body["tool_choice"], "auto");
            assert_eq!(body["parallel_tool_calls"], true);
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
            assert_eq!(body["text"], json!({"format":{"type":"text"}}));
            assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
            assert_eq!(body["prompt_cache_key"], "account-switch-contract");
            assert_eq!(
                body["instructions"],
                if mode.starts_with("bridge") {
                    "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy"
                } else {
                    "be brief"
                }
            );
        }
        let rows = runtime.usage_records().unwrap();
        let row = rows.last().unwrap();
        assert_eq!(
            row.conversation_identity.as_deref(),
            Some("codex:switch-session:switch-thread")
        );
        assert_eq!(row.upstream_attempted, Some(true));
        assert_eq!(
            row.execution_account_hash.as_deref(),
            Some(format!("sha256:{}", account_hash(&requests[0].0)).as_str())
        );
        let input_text = requests[0].1["input"].to_string();
        assert!(input_text.contains("first question"), "{input_text}");
        assert!(input_text.contains("answer from A"), "{input_text}");
        assert!(input_text.contains("follow-up question"), "{input_text}");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn oversize_full_input_detection_ignores_chained_and_small_frames() {
        let padding = "x".repeat(2048);
        let full = json!({"type":"response.create","input":[{"content":padding}]});
        assert!(oversize_full_input_frame_bytes(&full, 1024).is_some());
        assert!(oversize_full_input_frame_bytes(&full, 1024 * 1024).is_none());
        assert!(oversize_full_input_frame_bytes(&full, 0).is_none());
        let mut chained = full.clone();
        chained["previous_response_id"] = json!("resp_1");
        assert!(oversize_full_input_frame_bytes(&chained, 1024).is_none());
        chained["previous_response_id"] = json!("");
        assert!(oversize_full_input_frame_bytes(&chained, 1024).is_some());
    }

    #[test]
    fn close_frame_description_includes_code_and_reason() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
        let frame = CloseFrame {
            code: CloseCode::Size,
            reason: "too big".into(),
        };
        assert_eq!(
            describe_close_frame(Some(&frame)),
            "code=1009 reason=\"too big\""
        );
        assert_eq!(describe_close_frame(None), "code=none reason=none");
    }

    /// A full-input frame above the threshold must never reach the upstream
    /// WebSocket (chatgpt.com drops it); it is served over HTTP with the body
    /// untouched, and a chained follow-up on the same connection stays on HTTP
    /// because a fresh socket never saw that response id. A small full-input
    /// frame still uses the socket.
    #[tokio::test]
    async fn oversize_full_input_frame_is_served_over_http() {
        use axum::extract::Json;
        let ws_frames = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let http_bodies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let ws_capture = ws_frames.clone();
        let http_capture = http_bodies.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let frames = ws_capture.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                            frames.lock().unwrap().push(serde_json::from_str(&text).unwrap());
                            let event = json!({"type":"response.completed","response":{"id":"resp_ws","status":"completed","output":[]}});
                            socket.send(AxumWsMessage::Text(event.to_string().into())).await.unwrap();
                        }
                    })
                }
            })
            .post(move |Json(body): Json<Value>| {
                let bodies = http_capture.clone();
                async move {
                    bodies.lock().unwrap().push(body);
                    axum::Json(json!({"id":"resp_http","object":"response","status":"completed","output":[]}))
                }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
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
        async fn complete(
            client: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            frame: Value,
        ) -> Value {
            client
                .send(TungsteniteMessage::Text(frame.to_string().into()))
                .await
                .unwrap();
            loop {
                let message =
                    tokio::time::timeout(std::time::Duration::from_secs(20), client.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                let TungsteniteMessage::Text(text) = message else {
                    continue;
                };
                let event: Value = serde_json::from_str(&text).unwrap();
                match event["type"].as_str() {
                    Some("response.completed") => return event,
                    Some("response.failed" | "error") => panic!("turn failed: {event}"),
                    _ => {}
                }
            }
        }

        // A socket is already open (and owns resp_ws) before the oversize frame.
        let first = complete(
            &mut client,
            json!({"type":"response.create","model":"vellum-official","stream":false,
                "input":[{"role":"user","content":"warm"}]}),
        )
        .await;
        assert_eq!(first["response"]["id"], "resp_ws");
        assert_eq!(ws_frames.lock().unwrap().len(), 1);

        let padding = "x".repeat(OFFICIAL_WS_FULL_INPUT_HTTP_THRESHOLD_BYTES + 1024);
        let big = complete(
            &mut client,
            json!({"type":"response.create","model":"vellum-official","stream":false,
                "prompt_cache_key":"oversize-cache",
                "input":[{"role":"user","content":padding}]}),
        )
        .await;
        assert_eq!(big["response"]["id"], "resp_http");
        assert_eq!(ws_frames.lock().unwrap().len(), 1);
        {
            let bodies = http_bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0]["prompt_cache_key"], "oversize-cache");
            assert_eq!(
                bodies[0]["input"][0]["content"].as_str().unwrap().len(),
                padding.len()
            );
            assert!(bodies[0].get("type").is_none());
        }

        let chained = complete(
            &mut client,
            json!({"type":"response.create","model":"vellum-official","stream":false,
                "previous_response_id":"resp_http",
                "input":[{"role":"user","content":"next"}]}),
        )
        .await;
        assert_eq!(chained["response"]["id"], "resp_http");
        assert_eq!(
            ws_frames.lock().unwrap().len(),
            1,
            "a chain created over HTTP must not be sent to the open socket"
        );
        assert_eq!(http_bodies.lock().unwrap().len(), 2);

        let small = complete(
            &mut client,
            json!({"type":"response.create","model":"vellum-official","stream":false,
                "input":[{"role":"user","content":"small"}]}),
        )
        .await;
        assert_eq!(small["response"]["id"], "resp_ws");
        assert_eq!(ws_frames.lock().unwrap().len(), 2);
        assert_eq!(http_bodies.lock().unwrap().len(), 2);

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
    async fn pooled_pre_output_quota_retries_on_other_socket_once() {
        run_pool_quota_retry("success").await;
        run_pool_quota_retry("twice").await;
        run_pool_quota_retry("handshake").await;
        run_pool_quota_retry("continuation").await;
    }

    #[tokio::test]
    async fn pooled_quota_after_output_is_forwarded_without_retry() {
        run_pool_quota_retry("after_output").await;
    }

    #[tokio::test]
    async fn pooled_quota_without_fitting_alternative_is_forwarded() {
        run_pool_quota_retry("no_fit").await;
    }

    #[tokio::test]
    async fn pooled_quota_retries_long_chained_active_turn_on_other_socket() {
        run_pool_quota_retry("long_continuation").await;
    }

    #[tokio::test]
    async fn pooled_quota_with_missing_history_marks_exhausted_and_forwards_error() {
        run_pool_quota_retry("missing_history").await;
    }

    async fn run_pool_quota_retry(mode: &'static str) {
        let continuation_turns = match mode {
            "long_continuation" => 40,
            "continuation" => 1,
            _ => 0,
        };
        use crate::config::ProxyRuntimeIdentity;
        use crate::credentials::{CredentialProvider, MemoryCredentialProvider};
        use crate::diagnostics::RuntimeDiagnostics;
        use crate::environment::ExecutionEnvironment;
        use crate::official_auth::{
            OfficialAuthDecision, OfficialAuthProvider, OfficialAuthorization,
            OfficialRoutingContext,
        };
        use crate::state::{ModelRouteView, StaticRouteCatalog};

        struct PoolAuth {
            mode: &'static str,
            calls: Arc<Mutex<Vec<(String, bool)>>>,
        }
        fn authorization(id: &str) -> OfficialAuthorization {
            OfficialAuthorization {
                access_token: format!("test-{id}"),
                account_id: Some(id.into()),
                selection_revision: Some(1),
                selection_verified: true,
            }
        }
        #[async_trait::async_trait]
        impl OfficialAuthProvider for PoolAuth {
            async fn authorize(&self, _: &str) -> Result<OfficialAuthDecision, String> {
                Ok(OfficialAuthDecision::Managed(authorization("account-a")))
            }
            async fn refresh_after_rejection(
                &self,
                _: &str,
                _: &OfficialAuthorization,
            ) -> Result<OfficialAuthorization, String> {
                Err("unexpected refresh".into())
            }
            async fn retry_after_quota(
                &self,
                _: &str,
                hash: &str,
                reset: Option<u64>,
                context: &OfficialRoutingContext,
                allow: bool,
            ) -> Result<Option<OfficialAuthorization>, String> {
                assert_eq!(reset, Some(1_900_000_000_000));
                assert!(context.estimated_input_tokens > 0);
                self.calls.lock().unwrap().push((hash.into(), allow));
                Ok((allow && self.mode != "no_fit").then(|| authorization("account-b")))
            }
        }
        struct PoolState {
            base: StaticProxyState,
            runtime: Arc<ProxyRuntime>,
        }
        #[async_trait::async_trait]
        impl ProxyRuntimeState for PoolState {
            fn identity(&self) -> ProxyRuntimeIdentity {
                self.base.identity()
            }
            fn active_model_routes(&self) -> Vec<ModelRouteView> {
                self.base.active_model_routes()
            }
            fn route_for_model(&self, id: &str) -> Option<ModelRouteView> {
                self.base.route_for_model(id)
            }
            fn diagnostics(&self) -> RuntimeDiagnostics {
                self.base.diagnostics()
            }
            async fn credentials(&self) -> Arc<dyn CredentialProvider> {
                self.base.credentials().await
            }
            fn proxy_runtime(&self) -> Arc<ProxyRuntime> {
                self.runtime.clone()
            }
            fn execution_environment(&self) -> ExecutionEnvironment {
                self.base.execution_environment()
            }
        }
        let accounts = Arc::new(Mutex::new(Vec::<String>::new()));
        let frames = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured_accounts = accounts.clone();
        let captured_frames = frames.clone();
        let upstream = Router::new().route("/backend-api/codex/responses", get(move |headers: HeaderMap, ws: WebSocketUpgrade| {
            let accounts = captured_accounts.clone();
            let frames = captured_frames.clone();
            async move {
                let account = headers["chatgpt-account-id"].to_str().unwrap().to_owned();
                accounts.lock().unwrap().push(account.clone());
                if mode == "handshake" && account == "account-a" {
                    return axum::response::IntoResponse::into_response((axum::http::StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(json!({"error":{"type":"usage_limit_reached", "resets_at":1_900_000_000_u64}}))));
                }
                ws.on_upgrade(move |mut socket| async move {
                    while let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                        let body: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(body["type"], "response.create");
                        assert_eq!(body["model"], "gpt-upstream");
                        assert_eq!(body["tools"], json!([{"type":"function", "name":"test_tool", "parameters":{"type":"object"}}]));
                        let attempt = {
                            let mut frames = frames.lock().unwrap();
                            frames.push(body);
                            frames.len()
                        };
                        if mode == "after_output" {
                            socket.send(AxumWsMessage::Text(json!({"type":"response.output_text.delta", "delta":"started"}).to_string().into())).await.unwrap();
                        }
                        let event = if attempt <= continuation_turns {
                            json!({"type":"response.completed", "response":{"id":format!("resp_before_quota_{attempt}"), "status":"completed",
                                "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"prior answer"}]}]}})
                        } else if account == "account-b" && matches!(mode, "success" | "handshake" | "continuation" | "long_continuation") {
                            json!({"type":"response.completed", "response":{"id":"resp_retry", "status":"completed", "output":[]}})
                        } else {
                            json!({"type":"error", "status":429, "error":{"type":"usage_limit_reached", "code":"usage_limit_reached", "message":"The usage limit has been reached", "resets_at":1_900_000_000_u64}})
                        };
                        socket.send(AxumWsMessage::Text(event.to_string().into())).await.unwrap();
                    }
                })
            }
        }).post(|| async { panic!("quota retry must stay on WebSocket"); #[allow(unreachable_code)] axum::http::StatusCode::INTERNAL_SERVER_ERROR }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let mut route = official_route_config(format!("http://{address}/backend-api/codex"));
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        let config = ProxyRuntimeConfig {
            models: vec![route],
            ..Default::default()
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runtime = Arc::new(ProxyRuntime::new(
            Arc::new(StaticRouteCatalog::from_config(&config)),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(PoolAuth {
                mode,
                calls: calls.clone(),
            }),
            Arc::new(crate::lifecycle::CountingRequestLifecycle::new()),
            Arc::new(crate::transport::ReqwestTransport::new()),
        ));
        let mut base = StaticProxyState::from_config(config).unwrap();
        base.mark_listener_ready();
        let state = Arc::new(PoolState {
            base,
            runtime: runtime.clone(),
        });
        let proxy = build_headless_router(state, InboundAccessPolicy::test_only_disabled());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            axum::serve(listener, proxy).await.unwrap();
        });
        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/v1/responses"))
                .await
                .unwrap();
        let mut request = json!({"type":"response.create", "model":"vellum-official",
            "input":[{"role":"user","content":"same turn"}], "stream":true,
            "client_metadata":{"x-codex-turn-metadata":{"session_id":"quota-retry", "thread_id":"long-thread", "root_turn_id":"running-root"}},
            "tools":[{"type":"function", "name":"test_tool", "parameters":{"type":"object"}}]
        });
        if mode == "missing_history" {
            request["previous_response_id"] = json!("resp_not_in_local_history");
        }
        for n in 1..=continuation_turns {
            let mut first = request.clone();
            first["input"][0]["content"] = if mode == "long_continuation" && n == 1 {
                json!(format!("prior question {}", "x".repeat(900_000)))
            } else {
                json!("prior question")
            };
            client
                .send(TungsteniteMessage::Text(first.to_string().into()))
                .await
                .unwrap();
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                websocket_event(&frame).unwrap()["response"]["id"],
                format!("resp_before_quota_{n}")
            );
            request["previous_response_id"] = json!(format!("resp_before_quota_{n}"));
        }
        client
            .send(TungsteniteMessage::Text(request.to_string().into()))
            .await
            .unwrap();
        let mut received = Vec::new();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let TungsteniteMessage::Text(text) = frame {
                let event: Value = serde_json::from_str(&text).unwrap();
                let terminal =
                    matches!(event["type"].as_str(), Some("error" | "response.completed"));
                received.push(event);
                if terminal {
                    break;
                }
            }
        }
        let retries = matches!(
            mode,
            "success" | "twice" | "handshake" | "continuation" | "long_continuation"
        );
        assert_eq!(
            *accounts.lock().unwrap(),
            if retries {
                vec!["account-a", "account-b"]
            } else {
                vec!["account-a"]
            }
        );
        let frames = frames.lock().unwrap().clone();
        assert_eq!(
            frames.len(),
            if continuation_turns > 0 {
                continuation_turns + 2
            } else if matches!(mode, "success" | "twice") {
                2
            } else {
                1
            }
        );
        if frames.len() == 2 {
            assert_eq!(frames[0]["input"], frames[1]["input"]);
        }
        if continuation_turns > 0 {
            let replay = frames.last().unwrap();
            assert!(replay.get("previous_response_id").is_none());
            let text = replay["input"].to_string();
            for expected in ["prior question", "prior answer", "same turn"] {
                assert!(text.contains(expected), "{text}");
            }
            assert!(replay["input"].as_array().unwrap().len() > continuation_turns * 2);
            if mode == "long_continuation" {
                assert!(text.len() > 900_000);
            }
        }
        assert_eq!(received.len(), if mode == "after_output" { 2 } else { 1 });
        assert_eq!(
            received.last().unwrap()["type"],
            if matches!(
                mode,
                "success" | "handshake" | "continuation" | "long_continuation"
            ) {
                "response.completed"
            } else {
                "error"
            }
        );
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), if mode == "twice" { 2 } else { 1 });
        assert_eq!(
            calls[0].1,
            !matches!(mode, "after_output" | "missing_history")
        );
        if mode == "twice" {
            assert!(!calls[1].1, "second 429 must not retry");
        }
        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
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

    #[test]
    fn portable_warmup_only_resumes_the_turn_chained_onto_it() {
        let (warmup, response) = portable_warmup_response(&json!({
            "model": "m",
            "input": [{"role": "user", "content": "a"}],
            "generate": false
        }));
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"], json!([]));

        let mut pending = Some(warmup);
        let mut unrelated = json!({"previous_response_id": "resp_other", "input": []});
        resume_portable_warmup(&mut unrelated, &mut pending);
        assert!(pending.is_none(), "a non-chained turn discards the prewarm");
        assert_eq!(unrelated["previous_response_id"], "resp_other");

        let (warmup, _) = portable_warmup_response(&json!({
            "input": [{"role": "user", "content": "a"}]
        }));
        assert_ne!(response["id"], warmup.response_id.as_str());
        let mut chained = json!({
            "previous_response_id": warmup.response_id,
            "input": [{"role": "user", "content": "b"}]
        });
        let mut pending = Some(warmup);
        resume_portable_warmup(&mut chained, &mut pending);
        assert!(chained.get("previous_response_id").is_none());
        assert_eq!(
            chained["input"],
            json!([
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"}
            ])
        );
    }

    /// Codex prewarms a WebSocket turn with `generate=false`, then chains the
    /// real turn onto the prewarm id with only the new input. On the portable
    /// HTTP path neither field may reach the provider: HTTP Responses rejects
    /// `generate`, and the prewarm id exists nowhere upstream.
    #[tokio::test]
    async fn portable_websocket_answers_prewarm_locally_and_restores_its_input() {
        run_portable_prewarm(false).await;
    }

    #[tokio::test]
    async fn official_websocket_bridge_answers_prewarm_locally() {
        run_portable_prewarm(true).await;
    }

    async fn run_portable_prewarm(official: bool) {
        use axum::extract::Json;
        use axum::routing::post;

        let bodies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let bodies_srv = Arc::clone(&bodies);
        let upstream = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let bodies = Arc::clone(&bodies_srv);
                async move {
                    bodies.lock().unwrap().push(body);
                    axum::Json(json!({
                        "id": "resp_third_party",
                        "object": "response",
                        "status": "completed",
                        "output": []
                    }))
                }
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream).await;
        });

        let route = RuntimeRouteConfig {
            route_id: "third-party".into(),
            catalog_id: "vlm-third-party".into(),
            name: "ThirdParty".into(),
            base_url: format!("http://{upstream_address}/v1"),
            provider_kind: if official {
                RuntimeProviderKind::Official
            } else {
                RuntimeProviderKind::OpenAiCompatible
            },
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
        async fn send_and_complete(
            client: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            frame: Value,
        ) -> Value {
            client
                .send(TungsteniteMessage::Text(frame.to_string().into()))
                .await
                .unwrap();
            loop {
                let message =
                    tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                let TungsteniteMessage::Text(text) = message else {
                    continue;
                };
                let event: Value = serde_json::from_str(&text).unwrap();
                match event["type"].as_str() {
                    Some("response.completed") => return event,
                    Some("response.failed") => panic!("turn failed: {event}"),
                    _ => {}
                }
            }
        }

        let tools = json!([serde_json::from_str::<Value>(include_str!(
            "../tests/fixtures/code-mode-exec.json"
        ))
        .unwrap()]);
        let warmup = send_and_complete(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vlm-third-party",
                "instructions": "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy",
                "input": [{"role": "user", "content": "first"}],
                "stream": false,
                "tools": [], "tool_choice": "none",
                "generate": false
            }),
        )
        .await;
        let warmup_id = warmup["response"]["id"].as_str().unwrap().to_string();
        assert!(
            bodies.lock().unwrap().is_empty(),
            "prewarm must not reach upstream"
        );

        let turn = send_and_complete(
            &mut client,
            json!({
                "type": "response.create",
                "model": "vlm-third-party",
                "instructions": "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy",
                "previous_response_id": warmup_id,
                "input": [{"role": "user", "content": "second"}],
                "stream": false,
                "tools": tools,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"],
                "text": {"format":{"type":"text"}},
                "reasoning": {"effort":"high","summary":"auto"},
                "prompt_cache_key":"bridge-contract"
            }),
        )
        .await;
        assert_eq!(turn["response"]["id"], "resp_third_party");

        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1, "{bodies:?}");
        let upstream_body = &bodies[0];
        if official {
            assert_eq!(upstream_body["tools"], tools);
            assert_eq!(upstream_body["tool_choice"], "auto");
            assert_eq!(upstream_body["parallel_tool_calls"], true);
            assert_eq!(
                upstream_body["include"],
                json!(["reasoning.encrypted_content"])
            );
            assert_eq!(upstream_body["text"], json!({"format":{"type":"text"}}));
            assert_eq!(
                upstream_body["reasoning"],
                json!({"effort":"high","summary":"auto"})
            );
            assert_eq!(upstream_body["prompt_cache_key"], "bridge-contract");
            assert_eq!(upstream_body["instructions"],
                "You are judging one planned coding-agent action. # User Authorization Scoring # Outcome Policy");
        }
        assert!(upstream_body.get("generate").is_none(), "{upstream_body}");
        assert!(
            upstream_body.get("previous_response_id").is_none(),
            "{upstream_body}"
        );
        let text = upstream_body["input"].to_string();
        let first = text.find("first").expect("prewarm input restored");
        let second = text.find("second").expect("turn input kept");
        assert!(first < second, "{text}");

        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
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
    async fn native_prewarm_during_inference_never_becomes_phantom_usage() {
        run_native_prewarm_during_inference(false).await;
    }

    #[tokio::test]
    async fn native_prewarm_during_inference_does_not_block_next_real_turn() {
        run_native_prewarm_during_inference(true).await;
    }

    async fn run_native_prewarm_during_inference(next_turn: bool) {
        let first_seen = Arc::new(tokio::sync::Notify::new());
        let closed = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(AtomicUsize::new(0));
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get({
                let first_seen = first_seen.clone();
                let closed = closed.clone();
                let seen = seen.clone();
                move |ws: WebSocketUpgrade| {
                    let first_seen = first_seen.clone();
                    let closed = closed.clone();
                    let seen = seen.clone();
                    async move {
                        ws.on_upgrade(move |mut socket| async move {
                            let Some(Ok(AxumWsMessage::Text(_))) = socket.recv().await else {
                                return;
                            };
                            seen.fetch_add(1, Ordering::SeqCst);
                            first_seen.notify_one();
                            // Keep inference in flight while the client sends prewarm.
                            // This upstream deliberately never answers a warmup frame.
                            if let Ok(Some(Ok(AxumWsMessage::Text(_)))) = tokio::time::timeout(
                                std::time::Duration::from_millis(100),
                                socket.recv(),
                            )
                            .await
                            {
                                seen.fetch_add(1, Ordering::SeqCst);
                            }
                            socket
                                .send(AxumWsMessage::Text(
                                    json!({"type":"response.completed","response":{
                        "id":"resp_real","status":"completed","output":[]}})
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                            if next_turn {
                                let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await
                                else {
                                    panic!("missing next real turn");
                                };
                                let body: Value = serde_json::from_str(&text).unwrap();
                                assert!(body.get("generate").is_none());
                                assert!(body.get("previous_response_id").is_none());
                                assert_eq!(
                                    body["input"],
                                    json!([
                                        {"role":"user","content":"warmup"},
                                        {"role":"user","content":"next turn"}
                                    ])
                                );
                                seen.fetch_add(1, Ordering::SeqCst);
                                socket
                                    .send(AxumWsMessage::Text(
                                        json!({"type":"response.completed","response":{
                            "id":"resp_next","status":"completed","output":[]}})
                                        .to_string()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            socket.send(AxumWsMessage::Close(None)).await.unwrap();
                            closed.notify_one();
                        })
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });
        let mut state = StaticProxyState::from_config(ProxyRuntimeConfig {
            models: vec![official_route_config(format!(
                "http://{address}/backend-api/codex"
            ))],
            ..Default::default()
        })
        .unwrap();
        state.mark_listener_ready();
        let state = Arc::new(state);
        let runtime = state.proxy_runtime();
        let proxy = build_headless_router(state, InboundAccessPolicy::test_only_disabled());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let _ = axum::serve(listener, proxy).await;
        });
        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/v1/responses"))
                .await
                .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({"type":"response.create","model":"vellum-official",
            "input":[{"role":"user","content":"real turn"}],"stream":true})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), first_seen.notified())
            .await
            .unwrap();
        client
            .send(TungsteniteMessage::Text(
                json!({"type":"response.create","model":"vellum-official",
            "input":[{"role":"user","content":"warmup"}],"generate":false})
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
                if event.pointer("/response/id").and_then(Value::as_str) == Some("resp_real") {
                    break;
                }
            }
        }
        if next_turn {
            let warmup_id = loop {
                let frame = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if let TungsteniteMessage::Text(text) = frame {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    if event["type"] == "response.completed" {
                        break event["response"]["id"].as_str().unwrap().to_owned();
                    }
                }
            };
            client
                .send(TungsteniteMessage::Text(
                    json!({"type":"response.create","model":"vellum-official",
                "previous_response_id":warmup_id,"input":[{"role":"user","content":"next turn"}],
                "stream":true})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(frame) = client.next().await {
                if matches!(frame.unwrap(), TungsteniteMessage::Close(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), closed.notified())
            .await
            .unwrap();
        let rows = runtime.usage_records().unwrap();
        let expected_turns = if next_turn { 2 } else { 1 };
        assert_eq!(
            rows.len(),
            expected_turns,
            "prewarm must not create a failed usage row: {rows:?}"
        );
        assert!(rows.iter().all(|row| row.status == 200), "{rows:?}");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            expected_turns,
            "prewarm must stay local"
        );
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn official_server_error_type_and_code_are_retryable_provider_failures() {
        for (field, value) in [
            ("type", "server_error"),
            ("type", "api_error"),
            ("type", "internal_error"),
            ("type", "service_unavailable"),
            ("code", "server_error"),
            ("code", "internal_error"),
        ] {
            let mut error = json!({"message":"An error occurred while processing your request. You can retry your request."});
            error[field] = json!(value);
            for event in [
                json!({"type":"error","error":error.clone()}),
                json!({"type":"response.failed","response":{"error":error.clone()}}),
            ] {
                run_official_error_frame(event, "provider_unavailable", 503).await;
            }
        }
    }

    #[tokio::test]
    async fn official_websocket_top_level_error_is_terminal_invalid_request() {
        run_official_error_frame(
            json!({"type":"error","status":400,"error":{
            "type":"invalid_request_error","message":"Invalid `previous_response_id`."}}),
            "invalid_request",
            400,
        )
        .await;
    }

    #[tokio::test]
    async fn official_content_policy_rejection_is_fatal_without_frame_rewriting() {
        run_official_error_frame(
            json!({"type":"response.failed","response":{"error":{
            "type":"invalid_request_error","code":"content_policy_violation",
            "message":"This content was flagged for possible cybersecurity risk."}}}),
            "invalid_request",
            400,
        )
        .await;
    }

    #[tokio::test]
    async fn official_connection_lifetime_limit_keeps_codex_retry_code() {
        run_official_error_frame(
            json!({"type":"error","status":400,"error":{
            "type":"invalid_request_error","code":"websocket_connection_limit_reached",
            "message":"Responses websocket connection limit reached (60 minutes)"}}),
            "provider_unavailable",
            503,
        )
        .await;
    }

    #[tokio::test]
    async fn official_usage_limit_frame_is_recorded_as_quota() {
        run_official_error_frame(
            json!({"type":"error","status":429,"error":{
            "type":"usage_limit_reached","plan_type":"plus","resets_at":1_791_900_000_i64,
            "message":"The usage limit has been reached"}}),
            "provider_quota",
            429,
        )
        .await;
    }

    async fn run_official_error_frame(expected: Value, category: &str, status: u16) {
        let frame = expected.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let upstream_attempts = attempts.clone();
        let upstream = Router::new().route(
            "/backend-api/codex/responses",
            get(move |ws: WebSocketUpgrade| {
                let frame = frame.clone();
                let attempts = upstream_attempts.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            let AxumWsMessage::Text(text) = message else {
                                continue;
                            };
                            let value: Value = serde_json::from_str(&text).unwrap();
                            if value.get("type").and_then(Value::as_str) == Some("response.create")
                            {
                                attempts.fetch_add(1, Ordering::SeqCst);
                                socket
                                    .send(AxumWsMessage::Text(frame.to_string().into()))
                                    .await
                                    .unwrap();
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
        assert_eq!(event, expected, "upstream frame must survive verbatim");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if runtime
                    .usage_records()
                    .unwrap()
                    .iter()
                    .any(|record| record.error_category.as_deref() == Some(category))
                {
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
            .find(|record| record.error_category.as_deref() == Some(category))
            .unwrap();
        assert_eq!(record.status, status, "{record:?}");
        assert_eq!(
            record.outcome.as_deref(),
            Some(
                if category == "provider_protocol" || category == "invalid_request" {
                    "protocol_failure"
                } else {
                    "provider_failure"
                }
            )
        );
        assert_eq!(record.upstream_attempted, Some(true));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "native errors must not trigger a hidden retry or account change"
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

    async fn assert_portable_upstream_rejection(
        status: u16,
        expected_status: u16,
        expected_code: &str,
    ) {
        use axum::http::StatusCode;
        use axum::routing::post;

        let upstream = Router::new().route(
            "/v1/responses",
            post(move || async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(json!({"error": {"message": "Unsupported parameter: previous_response_id"}})),
                )
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
            .expect("upstream rejection must fail the turn immediately")
            .unwrap()
            .unwrap();
        let TungsteniteMessage::Text(text) = failed else {
            panic!("expected response.failed text, got {failed:?}");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "response.failed");
        assert_eq!(event["response"]["error"]["code"], expected_code);
        assert!(event["response"]["error"]["code"].is_string());
        assert_eq!(event["response"]["error"]["status"], expected_status);
        if expected_status == 400 {
            assert_eq!(event["response"]["error"]["upstream_status"], status);
        }
        assert_eq!(event["response"]["error"]["category"], "provider_protocol");
        assert!(
            event["response"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(&status.to_string())),
            "{event}"
        );
        let records = runtime.usage_records().unwrap();
        assert!(
            records.iter().any(|record| {
                record.route_id == "grok-cli"
                    && record.status == expected_status
                    && record.error.is_some()
            }),
            "failed Grok turns must land in usage: {records:?}"
        );

        let http = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/responses"))
            .json(&json!({"model": "vlm-grok", "input": [{"role": "user", "content": "hi"}], "stream": false}))
            .send()
            .await
            .unwrap();
        assert_eq!(http.status().as_u16(), expected_status);
        let payload: Value = http.json().await.unwrap();
        assert_eq!(payload["error"]["code"], expected_status);
        assert_eq!(payload["error"]["category"], "provider_protocol");
        if expected_status == 400 {
            assert_eq!(payload["error"]["upstream_status"], status);
        }
        let streaming = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/responses"))
            .json(&json!({"model": "vlm-grok", "input": [{"role": "user", "content": "hi"}], "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(streaming.status().as_u16(), 200);
        let transcript = streaming.text().await.unwrap();
        let event: Value = transcript
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == "response.failed")
            .expect("terminal failure event");
        assert_eq!(event["response"]["error"]["code"], expected_code);
        assert!(event["response"]["error"]["code"].is_string());
        assert_eq!(event["response"]["error"]["status"], expected_status);
        let _ = client.close(None).await;
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn third_party_websocket_upstream_400_is_a_string_invalid_prompt_and_http_400() {
        assert_portable_upstream_rejection(400, 400, "invalid_prompt").await;
        assert_portable_upstream_rejection(422, 400, "invalid_prompt").await;
    }

    #[tokio::test]
    async fn third_party_websocket_surfaces_http_426_as_response_failed() {
        for status in [408, 409, 425, 426] {
            assert_portable_upstream_rejection(status, 502, "provider_protocol").await;
        }
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
    fn handoff_replay_summary_names_shape_without_content() {
        let original = json!({"type":"response.create","model":"gpt","previous_response_id":"resp_old",
            "instructions":"","store":false,"input":[{"type":"function_call_output","call_id":"c","output":"secret output"}]});
        let replay = json!({"type":"response.create","model":"gpt","instructions":"","store":false,"input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"custom","name":"exec","format":{"definition":"secret grammar"}},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"followup_task","description":"secret description"}]}]},
            {"type":"message","id":"msg_01a148ce-f306-7eb3-a3c3-2915d30c9fb9","role":"user","content":"secret task"},
            {"type":"function_call_output","call_id":"c","output":"secret output"}]});
        let summary = handoff_replay_summary(&original, &replay);
        assert_eq!(
            summary,
            "keys=[type,model,instructions,store] changed=[previous_response_id] tools=[] \
             additional_tools=[id=none:[custom:exec,namespace:collaboration{function:followup_task}]] \
             input_items=3 items_without_id=2"
        );
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn handoff_rejection_is_recovered_only_before_output() {
        let failed = json!({"type":"error","status":400,"error":{"type":"invalid_request_error","message":"bad"}});
        let mut turn = OfficialPendingTurn {
            plan: None,
            request: json!({}),
            request_id: "r".into(),
            execution_id: "e".into(),
            conversation_key: "c".into(),
            _cancel_guard: None,
            cancel_rx: None,
            started: std::time::Instant::now(),
            request_forwarded_ms: None,
            first_byte_ms: None,
            first_downstream_ms: None,
            terminal_ms: None,
            recorded: false,
            output_forwarded: false,
            cancelled_at: None,
            quota_retried: false,
        };
        assert!(
            rejected_handoff_replay(&turn, &failed).is_none(),
            "an ordinary turn surfaces its error"
        );
        assert!(handoff_preamble_event(&json!({"type":"response.created"})));
        assert!(!handoff_preamble_event(
            &json!({"type":"response.output_item.added"})
        ));
        turn.output_forwarded = true;
        assert!(rejected_handoff_replay(&turn, &failed).is_none());
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
