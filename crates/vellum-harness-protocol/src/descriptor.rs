use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessId(pub String);

impl HarnessId {
    pub const CODEX: &'static str = "codex";
    pub const GROK_BUILD: &'static str = "grok-build";
    pub const QWEN_CODE: &'static str = "qwen-code";
    pub const DEEPSEEK_HARNESS: &'static str = "deepseek-harness";
    pub const VELLUM_GENERIC: &'static str = "vellum-generic";
    pub const ZCODE_DESKTOP: &'static str = "zcode-desktop";

    pub fn new(value: impl Into<String>) -> Result<Self, HarnessIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(HarnessIdError::Empty);
        }
        Ok(Self(value))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HarnessIdError {
    #[error("harness id must not be empty")]
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub display_name: String,
    pub vendor: String,
    pub transport: HarnessTransportKind,
    pub process_scope: ProcessScope,
    pub capabilities: HarnessCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HarnessTransportKind {
    CodexAppServer,
    AcpStdio,
    AcpHttp,
    VellumGeneric,
    ZcodeDesktopTap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessScope {
    PerSession,
    PerWorkspace,
    Shared,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCapabilities {
    pub session_resume: bool,
    pub session_list: bool,
    pub session_close: bool,
    pub model_selection: bool,
    pub reasoning_effort: bool,
    pub reasoning_stream: bool,
    pub tool_lifecycle: bool,
    pub permissions: bool,
    pub compaction: bool,
    pub plans: bool,
    pub commands: bool,
    pub terminals: bool,
    pub subagents: bool,
    pub native_memory: bool,
    pub usage: bool,
    pub context_usage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionAuthority {
    NativeHarness,
    CodexNative,
    VellumGeneric,
    Unsupported,
}
