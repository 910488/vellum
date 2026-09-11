//! Typed parser for Codex app-server notifications relayed through the
//! Remote Broker as `ThreadEvent`s.
//!
//! The broker stores the upstream JSON-RPC notification method verbatim
//! (`thread_actor::ingest_upstream_event`), so the methods here are the Codex
//! app-server protocol's own: `turn/started`, `turn/completed`, `item/started`,
//! `item/completed`, `item/commandExecution/outputDelta`, ... This is *not*
//! the Responses SSE surface (`response.output_item.*`) that the proxy
//! presents to Codex core; those events never appear on the Broker lane.
//!
//! Item payloads follow `codex-rs/app-server-protocol` v2
//! (`ThreadItem` with `#[serde(tag = "type")]`, `ItemStartedNotification`,
//! `ItemCompletedNotification`) plus a captured real `item/started`
//! commandExecution notification (values sanitized). The unit tests below
//! are not `#[ignore]`d and run in normal CI so a protocol drift fails here
//! instead of only on a live Jetson.

use serde_json::Value;
use vellum_remote_protocol::ThreadEvent;

#[cfg(test)]
use chrono::Utc;

/// App-server notification methods that carry a `ThreadItem` payload.
pub const ITEM_STARTED: &str = "item/started";
pub const ITEM_COMPLETED: &str = "item/completed";

/// `ThreadItem` wire tags (`#[serde(tag = "type", rename_all = "camelCase")]`).
pub const ITEM_TYPE_COMMAND_EXECUTION: &str = "commandExecution";
pub const ITEM_TYPE_AGENT_MESSAGE: &str = "agentMessage";
pub const ITEM_TYPE_USER_MESSAGE: &str = "userMessage";

/// `CommandExecutionStatus` wire values.
pub const COMMAND_STATUS_IN_PROGRESS: &str = "inProgress";
pub const COMMAND_STATUS_COMPLETED: &str = "completed";

/// The `item` payload carried by an `item/started` / `item/completed` event.
pub fn item(event: &ThreadEvent) -> Option<&Value> {
    if event.method != ITEM_STARTED && event.method != ITEM_COMPLETED {
        return None;
    }
    event.data.get("item")
}

/// The `ThreadItem.type` tag of an item event.
pub fn item_type(event: &ThreadEvent) -> Option<&str> {
    item(event)?.get("type")?.as_str()
}

/// The command string of a `commandExecution` item, when present.
pub fn command(event: &ThreadEvent) -> Option<&str> {
    item(event)?.get("command")?.as_str()
}

fn command_status(event: &ThreadEvent) -> Option<&str> {
    item(event)?.get("status")?.as_str()
}

fn exit_code(event: &ThreadEvent) -> Option<i64> {
    item(event)?.get("exitCode")?.as_i64()
}

/// A command the model asked the runtime to execute: an `item/started`
/// notification whose item is a `commandExecution` still in progress. A
/// `userMessage` echo of the prompt can never match because its item type is
/// `userMessage`, not `commandExecution`.
pub fn is_command_started(event: &ThreadEvent) -> bool {
    event.method == ITEM_STARTED
        && item_type(event) == Some(ITEM_TYPE_COMMAND_EXECUTION)
        && command_status(event) == Some(COMMAND_STATUS_IN_PROGRESS)
}

/// Only a *completed* command execution with a zero exit code and `needle`
/// inside the aggregated output counts as real tool execution. Failed,
/// cancelled or still-running commands never match, and neither does any
/// other item type (including a `userMessage` prompt echo).
pub fn is_completed_command_output(event: &ThreadEvent, needle: &str) -> bool {
    event.method == ITEM_COMPLETED
        && item_type(event) == Some(ITEM_TYPE_COMMAND_EXECUTION)
        && command_status(event) == Some(COMMAND_STATUS_COMPLETED)
        && exit_code(event) == Some(0)
        && item(event)
            .and_then(|item| item.get("aggregatedOutput"))
            .and_then(Value::as_str)
            .is_some_and(|output| output.contains(needle))
}

/// An assistant message item (`agentMessage`) whose text carries `needle`.
/// Only the model's own final message can produce this shape; a prompt echo
/// is a `userMessage` item, not an `agentMessage`.
pub fn is_agent_message_containing(event: &ThreadEvent, needle: &str) -> bool {
    event.method == ITEM_COMPLETED
        && item_type(event) == Some(ITEM_TYPE_AGENT_MESSAGE)
        && item(event)
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains(needle))
}

/// Turn-window filter over the append-only per-thread event log. The broker
/// seq is a per-thread append-only cursor, so `seq <= max_seq` selects the
/// first turn's events and `after_seq < seq <= max_seq` selects a later
/// turn's. Mirrors the live async scanner so unit tests can lock the window
/// semantics without a broker connection.
pub fn in_turn_window(
    event: &ThreadEvent,
    thread: &str,
    after_seq: Option<u64>,
    max_seq: u64,
) -> bool {
    event.thread_id == thread && event.seq <= max_seq && after_seq.is_none_or(|seq| event.seq > seq)
}

/// Synchronous scan of a seq-sorted event log: the last event matching
/// `predicate` inside the turn window. Unknown methods and item types are
/// simply never matched, so they cannot cause a false positive.
pub fn last_matching<'a>(
    events: &'a [ThreadEvent],
    thread: &str,
    after_seq: Option<u64>,
    max_seq: u64,
    predicate: impl Fn(&ThreadEvent) -> bool,
) -> Option<&'a ThreadEvent> {
    events
        .iter()
        .filter(|event| in_turn_window(event, thread, after_seq, max_seq) && predicate(event))
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const THREAD: &str = "t-live-01";
    const MARKER: &str = "VELLUM_E806_TOOL_OK";

    fn thread_event(method: &str, seq: u64, turn_id: &str, data: Value) -> ThreadEvent {
        let item_id = data
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        ThreadEvent {
            seq,
            thread_id: THREAD.into(),
            upstream_epoch: 1,
            method: method.into(),
            occurred_at: Utc::now(),
            turn_id: Some(turn_id.into()),
            item_id,
            data,
        }
    }

    /// The first-turn prompt echo: the user message containing the marker and
    /// the tool name. Mirrors the l3/l8 prompts.
    fn prompt_echo() -> ThreadEvent {
        thread_event(
            ITEM_STARTED,
            2,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "userMessage",
                    "id": "msg-user-1",
                    "content": [{
                        "type": "inputText",
                        "text": "Use shell_command to run printf VELLUM_E806_TOOL_OK, then report it."
                    }]
                }
            }),
        )
    }

    /// `item/started` commandExecution, shape aligned with the captured real
    /// notification (values sanitized).
    fn command_started() -> ThreadEvent {
        thread_event(
            ITEM_STARTED,
            4,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "commandExecution",
                    "id": "call_AEjlbHqLYNM7kbU3N6uw1CNi",
                    "command": "printf VELLUM_E806_TOOL_OK",
                    "cwd": "/tmp/project",
                    "processId": null,
                    "status": "inProgress",
                    "commandActions": [],
                    "aggregatedOutput": null,
                    "exitCode": null,
                    "durationMs": null
                }
            }),
        )
    }

    fn command_completed(status: &str, exit_code: Option<i64>) -> ThreadEvent {
        thread_event(
            ITEM_COMPLETED,
            5,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "commandExecution",
                    "id": "call_AEjlbHqLYNM7kbU3N6uw1CNi",
                    "command": "printf VELLUM_E806_TOOL_OK",
                    "cwd": "/tmp/project",
                    "processId": "p-1",
                    "status": status,
                    "commandActions": [],
                    "aggregatedOutput": if exit_code == Some(0) { MARKER.to_string() } else { String::new() },
                    "exitCode": exit_code,
                    "durationMs": 12
                }
            }),
        )
    }

    fn agent_message() -> ThreadEvent {
        thread_event(
            ITEM_COMPLETED,
            6,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "agentMessage",
                    "id": "msg-agent-1",
                    "text": "The marker is VELLUM_E806_TOOL_OK.",
                    "phase": "complete"
                }
            }),
        )
    }

    #[test]
    fn prompt_echo_cannot_impersonate_tool_call_or_tool_result() {
        let echo = prompt_echo();
        assert!(!is_command_started(&echo));
        assert!(!is_completed_command_output(&echo, MARKER));
        assert!(!is_agent_message_containing(&echo, MARKER));

        // The same echo arriving as item/completed is still not a tool result.
        let echo_completed = thread_event(
            ITEM_COMPLETED,
            3,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "userMessage",
                    "id": "msg-user-1",
                    "content": [{
                        "type": "inputText",
                        "text": "Use shell_command to run printf VELLUM_E806_TOOL_OK, then report it."
                    }]
                }
            }),
        );
        assert!(!is_command_started(&echo_completed));
        assert!(!is_completed_command_output(&echo_completed, MARKER));
        assert!(!is_agent_message_containing(&echo_completed, MARKER));
    }

    #[test]
    fn command_started_and_completed_output_match() {
        let started = command_started();
        assert!(is_command_started(&started));
        assert_eq!(command(&started), Some("printf VELLUM_E806_TOOL_OK"));
        assert!(!is_completed_command_output(&started, MARKER));

        let completed = command_completed(COMMAND_STATUS_COMPLETED, Some(0));
        assert!(is_completed_command_output(&completed, MARKER));
        assert!(!is_command_started(&completed));
    }

    #[test]
    fn only_completed_command_output_counts_as_execution() {
        // Still in progress: never counts.
        assert!(!is_completed_command_output(
            &command_completed(COMMAND_STATUS_IN_PROGRESS, None),
            MARKER
        ));
        // Failed with output: never counts.
        assert!(!is_completed_command_output(
            &command_completed("failed", Some(1)),
            MARKER
        ));
        // Completed but non-zero exit: never counts.
        assert!(!is_completed_command_output(
            &command_completed(COMMAND_STATUS_COMPLETED, Some(1)),
            MARKER
        ));
    }

    #[test]
    fn assistant_continuation_requires_structured_agent_message() {
        let message = agent_message();
        assert!(is_agent_message_containing(&message, MARKER));
        // A reasoning item with the marker is not an assistant message.
        let reasoning = thread_event(
            ITEM_COMPLETED,
            6,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "reasoning",
                    "id": "reason-1",
                    "summary": ["VELLUM_E806_TOOL_OK"],
                    "content": []
                }
            }),
        );
        assert!(!is_agent_message_containing(&reasoning, MARKER));
        // An agentMessage without the marker does not match either.
        let plain = thread_event(
            ITEM_COMPLETED,
            7,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "agentMessage",
                    "id": "msg-agent-2",
                    "text": "The answer is 1."
                }
            }),
        );
        assert!(!is_agent_message_containing(&plain, MARKER));
    }

    #[test]
    fn different_turn_markers_do_not_cross_match() {
        // Turn 1 runs seq 2..=10 (marker produced by a real command), turn 2
        // runs seq 11..=20 and restates the marker in an agentMessage.
        let log = vec![
            prompt_echo(),                                        // seq 2, turn-1
            command_started(),                                    // seq 4, turn-1
            command_completed(COMMAND_STATUS_COMPLETED, Some(0)), // seq 5, turn-1
            thread_event(
                ITEM_STARTED,
                11,
                "turn-2",
                json!({
                    "threadId": THREAD,
                    "turnId": "turn-2",
                    "item": {
                        "type": "userMessage",
                        "id": "msg-user-2",
                        "content": [{"type": "inputText", "text": "Continue: state the marker."}]
                    }
                }),
            ),
            thread_event(
                ITEM_COMPLETED,
                12,
                "turn-2",
                json!({
                    "threadId": THREAD,
                    "turnId": "turn-2",
                    "item": {
                        "type": "agentMessage",
                        "id": "msg-agent-2",
                        "text": "The marker is VELLUM_E806_TOOL_OK."
                    }
                }),
            ),
        ];

        // Turn-1 window (seq <= 10) finds the command result, never the
        // turn-2 message.
        let turn1_result = last_matching(&log, THREAD, None, 10, |event| {
            is_completed_command_output(event, MARKER)
        })
        .expect("turn 1 must contain the completed command output");
        assert_eq!(turn1_result.seq, 5);
        assert_eq!(turn1_result.turn_id.as_deref(), Some("turn-1"));
        assert!(last_matching(&log, THREAD, None, 10, |event| {
            is_agent_message_containing(event, MARKER)
        })
        .is_none());

        // Turn-2 window (10 < seq <= 20) finds only the continuation message.
        let turn2_message = last_matching(&log, THREAD, Some(10), 20, |event| {
            is_agent_message_containing(event, MARKER)
        })
        .expect("turn 2 must contain the agent message");
        assert_eq!(turn2_message.seq, 12);
        assert_eq!(turn2_message.turn_id.as_deref(), Some("turn-2"));
        assert!(last_matching(&log, THREAD, Some(10), 20, |event| {
            is_completed_command_output(event, MARKER)
        })
        .is_none());
    }

    #[test]
    fn unknown_app_server_events_are_ignored_without_misjudgment() {
        // A Responses SSE event (the wrong surface) must never match.
        let sse = thread_event(
            "response.output_item.done",
            1,
            "turn-1",
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call_output",
                    "id": "fc-1",
                    "output": MARKER,
                    "callId": "call-1"
                }
            }),
        );
        assert!(!is_command_started(&sse));
        assert!(!is_completed_command_output(&sse, MARKER));
        assert!(!is_agent_message_containing(&sse, MARKER));

        // Unknown app-server notifications and item types are also inert.
        let unknown_method = thread_event(
            "item/mcpToolCall/request",
            1,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {
                    "type": "mcpToolCall",
                    "id": "mcp-1",
                    "server": "s",
                    "tool": "t",
                    "arguments": {},
                    "status": "inProgress"
                }
            }),
        );
        assert!(!is_command_started(&unknown_method));
        assert!(!is_completed_command_output(&unknown_method, MARKER));
        assert!(!is_agent_message_containing(&unknown_method, MARKER));

        let unknown_item_type = thread_event(
            ITEM_COMPLETED,
            1,
            "turn-1",
            json!({
                "threadId": THREAD,
                "turnId": "turn-1",
                "item": {"type": "enteredReviewMode", "id": "review-1", "review": "r"}
            }),
        );
        assert!(!is_command_started(&unknown_item_type));
        assert!(!is_completed_command_output(&unknown_item_type, MARKER));
        assert!(!is_agent_message_containing(&unknown_item_type, MARKER));
    }
}
