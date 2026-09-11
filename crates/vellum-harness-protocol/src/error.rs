use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HarnessErrorCategory {
    RuntimeUnavailable,
    RuntimeCrashed,
    Transport,
    Protocol,
    Authentication,
    Permission,
    UnsupportedCapability,
    SessionNotFound,
    SessionConflict,
    Provider,
    ResourceBudget,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessErrorInfo {
    pub category: HarnessErrorCategory,
    pub message: String,
    pub diagnostic: Option<String>,
}
