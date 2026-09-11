//! Approval request and decision types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    Decline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalState {
    Pending,
    Presented,
    Accepted,
    Declined,
    Cancelled,
    Resolved,
    Orphaned,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub approval_token: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub method: String,
    pub state: ApprovalState,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub request: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalResponseRequest {
    pub approval_token: String,
    pub decision: ApprovalDecision,
}
