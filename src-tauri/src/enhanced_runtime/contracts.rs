//! Bridge-owned protocol dependencies, shared by routing and qualification.
pub const CONTRACT_VERSION: u32 = 2;
pub const REQUIRED_METHODS: &[&str] = &[
    "initialize",
    "thread/start",
    "thread/read",
    "thread/resume",
    "thread/fork",
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "turn/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/tool/call",
    "model/list",
    "account/read",
    "config/read",
];

/// Explicit response dependencies: response types have no method discriminator.
/// `true` means the native core writes the response; approvals travel backwards.
pub const RESPONSES: &[(&str, &str, bool)] = &[
    ("initialize", "InitializeResponse", true),
    ("thread/start", "ThreadStartResponse", true),
    ("thread/read", "ThreadReadResponse", true),
    ("thread/resume", "ThreadResumeResponse", true),
    ("thread/fork", "ThreadForkResponse", true),
    ("thread/list", "ThreadListResponse", true),
    ("turn/start", "TurnStartResponse", true),
    ("turn/steer", "TurnSteerResponse", true),
    ("turn/interrupt", "TurnInterruptResponse", true),
    (
        "item/commandExecution/requestApproval",
        "CommandExecutionRequestApprovalResponse",
        false,
    ),
    (
        "item/fileChange/requestApproval",
        "FileChangeRequestApprovalResponse",
        false,
    ),
    ("item/tool/call", "DynamicToolCallResponse", false),
];

pub fn is_remote_control(method: &str) -> bool {
    method.starts_with("remoteControl/")
}

pub fn is_global_official(method: &str) -> bool {
    matches!(method, "model/list" | "account/read" | "config/read")
}

/// These methods must never reach a runtime without a thread owner.
pub fn requires_thread(method: &str) -> bool {
    method.starts_with("turn/")
        || matches!(
            method,
            "thread/read"
                | "thread/resume"
                | "thread/fork"
                | "thread/unsubscribe"
                | "thread/rollback"
                | "thread/compact/start"
        )
}
