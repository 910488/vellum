//! The Codex App Server method surface, classified per §28 of the plan.
//!
//! Every name here is taken from the pinned schema under
//! `third_party/codex-app-server-schema/<version>`, not inferred from what a
//! facade would find convenient. `methods.rs` tests assert that each name below
//! actually exists in that schema, so an invented method fails the build rather
//! than reaching a UI that will never send it.

/// How the facade treats one Codex client request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Needed before a thread exists.
    RequiredBootstrap,
    /// Needed to open, restore or enumerate a thread.
    RequiredThread,
    /// Needed to run and interrupt a turn.
    RequiredTurn,
    /// Answered only when the bound harness declares the capability.
    OptionalFeature,
    /// Real Codex methods this facade does not serve. They are refused
    /// explicitly; a native harness has no equivalent to fake.
    Unsupported,
}

/// Bootstrap.
pub const INITIALIZE: &str = "initialize";
pub const MODEL_LIST: &str = "model/list";

/// Thread lifecycle.
pub const THREAD_START: &str = "thread/start";
pub const THREAD_RESUME: &str = "thread/resume";
pub const THREAD_READ: &str = "thread/read";
pub const THREAD_LIST: &str = "thread/list";

/// Turn lifecycle.
pub const TURN_START: &str = "turn/start";
pub const TURN_INTERRUPT: &str = "turn/interrupt";

/// Optional, capability-gated.
pub const THREAD_COMPACT_START: &str = "thread/compact/start";
pub const THREAD_NAME_SET: &str = "thread/name/set";
pub const THREAD_METADATA_UPDATE: &str = "thread/metadata/update";
pub const CONFIG_VALUE_WRITE: &str = "config/value/write";
pub const COMMAND_EXEC: &str = "command/exec";
pub const THREAD_FORK: &str = "thread/fork";

pub fn classify(method: &str) -> MethodClass {
    match method {
        INITIALIZE | MODEL_LIST => MethodClass::RequiredBootstrap,
        THREAD_START | THREAD_RESUME | THREAD_READ | THREAD_LIST => MethodClass::RequiredThread,
        TURN_START | TURN_INTERRUPT => MethodClass::RequiredTurn,
        THREAD_COMPACT_START
        | THREAD_NAME_SET
        | THREAD_METADATA_UPDATE
        | CONFIG_VALUE_WRITE
        | COMMAND_EXEC
        | THREAD_FORK => MethodClass::OptionalFeature,
        _ => MethodClass::Unsupported,
    }
}

/// Every method the facade names, for schema conformance checking.
pub const KNOWN_METHODS: &[&str] = &[
    INITIALIZE,
    MODEL_LIST,
    THREAD_START,
    THREAD_RESUME,
    THREAD_READ,
    THREAD_LIST,
    TURN_START,
    TURN_INTERRUPT,
    THREAD_COMPACT_START,
    THREAD_NAME_SET,
    THREAD_METADATA_UPDATE,
    CONFIG_VALUE_WRITE,
    COMMAND_EXEC,
    THREAD_FORK,
];

/// Server notifications the facade emits, all from the pinned schema.
pub mod notify {
    pub const THREAD_STARTED: &str = "thread/started";
    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const REASONING_TEXT_DELTA: &str = "item/reasoning/textDelta";
    pub const PLAN_UPDATED: &str = "turn/plan/updated";
    pub const TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
    pub const THREAD_COMPACTED: &str = "thread/compacted";
    pub const ERROR: &str = "error";

    pub const ALL: &[&str] = &[
        THREAD_STARTED,
        TURN_STARTED,
        TURN_COMPLETED,
        ITEM_STARTED,
        ITEM_COMPLETED,
        AGENT_MESSAGE_DELTA,
        REASONING_TEXT_DELTA,
        PLAN_UPDATED,
        TOKEN_USAGE_UPDATED,
        THREAD_COMPACTED,
        ERROR,
    ];
}

/// Server requests the facade sends to the UI and correlates replies for.
pub mod request {
    /// Permission approval. The UI answers with a JSON-RPC *response*, not a
    /// client request, which is why the facade needs a response inbox.
    pub const PERMISSIONS_REQUEST_APPROVAL: &str = "item/permissions/requestApproval";

    pub const ALL: &[&str] = &[PERMISSIONS_REQUEST_APPROVAL];
}

/// Thread item types, used when a harness event is surfaced as a thread item.
pub mod item {
    pub const AGENT_MESSAGE: &str = "agentMessage";
    pub const REASONING: &str = "reasoning";
    pub const DYNAMIC_TOOL_CALL: &str = "dynamicToolCall";
    pub const PLAN: &str = "plan";
    pub const SUB_AGENT_ACTIVITY: &str = "subAgentActivity";
    pub const CONTEXT_COMPACTION: &str = "contextCompaction";
}

/// `DynamicToolCallStatus` from the pinned schema.
pub mod tool_status {
    pub const IN_PROGRESS: &str = "inProgress";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
}
