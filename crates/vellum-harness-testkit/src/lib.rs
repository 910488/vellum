//! Deterministic fake ACP agent.
//!
//! Adapter and pipeline tests must not need a live provider. This agent speaks
//! enough of ACP to exercise session lifecycle, prompt turns, tool lifecycle,
//! permission server-requests and multi-session isolation, driven entirely by a
//! JSON script.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One scripted reaction to a `session/prompt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ScriptedEvent {
    TurnStarted {
        #[serde(default)]
        turn_id: Option<String>,
    },
    Reasoning {
        text: String,
    },
    ToolStart {
        tool_call_id: String,
        title: String,
    },
    ToolUpdate {
        tool_call_id: String,
        status: String,
    },
    ToolEnd {
        tool_call_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        output: Value,
        #[serde(default)]
        failed: bool,
    },
    /// Emitted as a server-originated JSON-RPC *request*, so the client must
    /// correlate and answer it rather than treat it as a notification.
    Permission {
        tool_call_id: String,
        title: String,
    },
    Assistant {
        text: String,
    },
    Usage {
        #[serde(default)]
        usage: Value,
    },
    /// A provider-specific event with no neutral equivalent.
    Native {
        method: String,
        #[serde(default)]
        payload: Value,
    },
    Complete {
        #[serde(default)]
        turn_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FakeAgentScript {
    #[serde(default)]
    pub on_prompt: Vec<ScriptedEvent>,
    /// Sessions the agent claims to already hold, for `session/load` and
    /// `session/list`.
    #[serde(default)]
    pub known_sessions: Vec<String>,
    /// Methods the fake agent answers with a JSON-RPC error, so capability
    /// degradation can be exercised end to end.
    #[serde(default)]
    pub unsupported_methods: Vec<String>,
}

impl FakeAgentScript {
    /// Loads from `argv[1]` (a script file) when given, otherwise from
    /// `VELLUM_FAKE_ACP_SCRIPT`. The file form is what parallel tests use, so
    /// they do not have to share one process-wide environment variable.
    pub fn from_process_args() -> Self {
        std::env::args()
            .nth(1)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .or_else(|| std::env::var("VELLUM_FAKE_ACP_SCRIPT").ok())
            .and_then(|source| serde_json::from_str(&source).ok())
            .unwrap_or_default()
    }
}

/// Frames the agent wants written back, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply(pub Vec<Value>);

/// The agent's protocol behaviour, isolated from any I/O so it can be unit
/// tested and also driven over real pipes by the `fake-acp-agent` binary.
#[derive(Debug, Default)]
pub struct FakeAcpAgent {
    script: FakeAgentScript,
    sessions: HashMap<String, u64>,
    next_session: u64,
    next_request_id: u64,
    last_prompt_session: Option<String>,
}

impl FakeAcpAgent {
    pub fn new(script: FakeAgentScript) -> Self {
        let mut agent = Self {
            sessions: script
                .known_sessions
                .iter()
                .map(|id| (id.clone(), 0))
                .collect(),
            script,
            next_session: 0,
            next_request_id: 9000,
            ..Default::default()
        };
        agent.next_session = agent.sessions.len() as u64;
        agent
    }

    /// Handles one inbound JSON-RPC frame.
    pub fn handle(&mut self, frame: &Value) -> Reply {
        // A frame with an id but no method is the client answering one of our
        // server requests, e.g. a permission decision. Echo it back as an
        // observable native event instead of replying to it.
        if frame.get("method").is_none() && frame.get("id").is_some() {
            let Some(session_id) = self.last_prompt_session.clone() else {
                return Reply(Vec::new());
            };
            let outcome = frame
                .get("result")
                .and_then(|result| result.get("outcome"))
                .cloned()
                .unwrap_or(Value::Null);
            return Reply(vec![json!({
                "jsonrpc": "2.0",
                "method": "testkit/permission_resolved",
                "params": {"sessionId": session_id, "outcome": outcome}
            })]);
        }
        let id = frame.get("id").cloned();
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame.get("params").cloned().unwrap_or(Value::Null);

        if self.script.unsupported_methods.iter().any(|m| m == method) {
            return Reply(id.into_iter().map(|id| error_frame(id, method)).collect());
        }

        match method {
            "initialize" => Reply(
                id.into_iter()
                    .map(|id| {
                        result_frame(id, json!({"protocolVersion": 1, "agentCapabilities": {}}))
                    })
                    .collect(),
            ),
            "session/new" => {
                self.next_session += 1;
                let session_id = format!("native-{}", self.next_session);
                self.sessions.insert(session_id.clone(), 0);
                Reply(
                    id.into_iter()
                        .map(|id| result_frame(id, json!({"sessionId": session_id})))
                        .collect(),
                )
            }
            "session/load" => {
                let session_id = session_of(&params);
                // Resume is only honoured for a session the agent still holds:
                // the client must never be able to conjure one back.
                if !self.sessions.contains_key(&session_id) {
                    return Reply(
                        id.into_iter()
                            .map(|id| error_frame(id, "unknown session"))
                            .collect(),
                    );
                }
                Reply(
                    id.into_iter()
                        .map(|id| result_frame(id, json!({"sessionId": session_id})))
                        .collect(),
                )
            }
            "session/list" => {
                let mut ids: Vec<&String> = self.sessions.keys().collect();
                ids.sort();
                let sessions: Vec<Value> = ids
                    .into_iter()
                    .map(|id| json!({"sessionId": id, "title": id}))
                    .collect();
                Reply(
                    id.into_iter()
                        .map(|id| result_frame(id, json!({"sessions": sessions})))
                        .collect(),
                )
            }
            "session/close" => {
                self.sessions.remove(&session_of(&params));
                Reply(
                    id.into_iter()
                        .map(|id| result_frame(id, json!({})))
                        .collect(),
                )
            }
            "session/prompt" => {
                let session_id = session_of(&params);
                self.last_prompt_session = Some(session_id.clone());
                // Every turn this agent runs is announced, so a test can prove
                // which harness actually owned it.
                let count = self.sessions.entry(session_id.clone()).or_insert(0);
                *count += 1;
                let mut frames = vec![json!({
                    "jsonrpc": "2.0",
                    "method": "testkit/prompt_observed",
                    "params": {"sessionId": session_id, "count": count}
                })];
                for event in self.script.on_prompt.clone() {
                    frames.extend(self.render(&session_id, event));
                }
                if let Some(id) = id {
                    frames.push(result_frame(id, json!({"stopReason": "end_turn"})));
                }
                Reply(frames)
            }
            _ => Reply(
                id.into_iter()
                    .map(|id| result_frame(id, json!({})))
                    .collect(),
            ),
        }
    }

    fn render(&mut self, session_id: &str, event: ScriptedEvent) -> Vec<Value> {
        let update = |body: Value| {
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session_id, "update": body}
            })
        };
        match event {
            ScriptedEvent::TurnStarted { turn_id } => {
                vec![update(
                    json!({"sessionUpdate":"turn_started","turnId":turn_id}),
                )]
            }
            ScriptedEvent::Reasoning { text } => vec![update(
                json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}}),
            )],
            ScriptedEvent::ToolStart {
                tool_call_id,
                title,
            } => vec![update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": title,
                "status": "pending"
            }))],
            ScriptedEvent::ToolUpdate {
                tool_call_id,
                status,
            } => vec![update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": status
            }))],
            ScriptedEvent::ToolEnd {
                tool_call_id,
                title,
                output,
                failed,
            } => vec![update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "title": title,
                "status": if failed { "failed" } else { "completed" },
                "rawOutput": output
            }))],
            ScriptedEvent::Permission {
                tool_call_id,
                title,
            } => {
                self.next_request_id += 1;
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": self.next_request_id,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session_id,
                        "toolCall": {"toolCallId": tool_call_id, "title": title},
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                        ]
                    }
                })]
            }
            ScriptedEvent::Assistant { text } => vec![update(
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
            )],
            ScriptedEvent::Usage { usage } => {
                vec![update(json!({"sessionUpdate":"usage","usage":usage}))]
            }
            ScriptedEvent::Native { method, payload } => {
                let mut params = match payload {
                    Value::Object(map) => map,
                    other => {
                        let mut map = Map::new();
                        map.insert("payload".into(), other);
                        map
                    }
                };
                params.insert("sessionId".into(), json!(session_id));
                vec![json!({"jsonrpc":"2.0","method":method,"params":Value::Object(params)})]
            }
            ScriptedEvent::Complete { turn_id } => {
                vec![update(
                    json!({"sessionUpdate":"turn_completed","turnId":turn_id}),
                )]
            }
        }
    }
}

fn session_of(params: &Value) -> String {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn result_frame(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_frame(id: Value, detail: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":detail}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> FakeAcpAgent {
        FakeAcpAgent::new(FakeAgentScript {
            on_prompt: vec![
                ScriptedEvent::Permission {
                    tool_call_id: "call-1".into(),
                    title: "write".into(),
                },
                ScriptedEvent::Assistant {
                    text: "done".into(),
                },
            ],
            ..Default::default()
        })
    }

    #[test]
    fn permission_is_emitted_as_a_server_request_not_a_notification() {
        let mut agent = agent();
        agent.handle(&json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}));
        let Reply(frames) = agent.handle(
            &json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"native-1"}}),
        );
        let permission = frames
            .iter()
            .find(|frame| {
                frame.get("method").and_then(Value::as_str) == Some("session/request_permission")
            })
            .expect("permission frame is emitted");
        // It carries an id, so the client has to answer it as a request.
        assert!(permission.get("id").is_some());
    }

    #[test]
    fn resume_of_a_session_the_agent_no_longer_holds_is_refused() {
        let mut agent = agent();
        let Reply(frames) = agent.handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"gone"}}),
        );
        assert!(frames[0].get("error").is_some());
    }
}
