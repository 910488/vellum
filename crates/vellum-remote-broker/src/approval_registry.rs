//! Pending server-initiated request registry.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use ulid::Ulid;
use vellum_remote_protocol::{ApprovalDecision, ApprovalState, PendingApproval};

use crate::app_server::jsonrpc::JsonRpcId;
use crate::event_store::StoreError;

#[derive(Clone)]
pub struct ApprovalRegistry {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct StoredApproval {
    pub approval: PendingApproval,
    pub upstream_epoch: u64,
    pub upstream_request_id: JsonRpcId,
}

impl ApprovalRegistry {
    pub fn from_shared(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        upstream_epoch: u64,
        upstream_request_id: JsonRpcId,
        method: impl Into<String>,
        request: Value,
        thread_id: Option<String>,
        turn_id: Option<String>,
        item_id: Option<String>,
    ) -> Result<StoredApproval, StoreError> {
        let token = format!("approval_{}", Ulid::new());
        let method = method.into();
        let created_at = Utc::now();
        let request_bytes = serde_json::to_vec(&request)?;
        let request_id_json = serde_json::to_vec(&upstream_request_id.to_value())?;
        let request_id_text = match &upstream_request_id {
            JsonRpcId::Number(n) => n.to_string(),
            JsonRpcId::String(s) => s.clone(),
        };
        let conn = self.conn.lock().expect("approval registry poisoned");
        conn.execute(
            "INSERT INTO pending_server_requests(
                local_token, upstream_epoch, upstream_request_id, upstream_request_id_json,
                thread_id, turn_id, item_id, method, state, request_ciphertext,
                response_ciphertext, created_at_ms, presented_at_ms, resolved_at_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, NULL, NULL)",
            params![
                token,
                upstream_epoch as i64,
                request_id_text,
                request_id_json,
                thread_id,
                turn_id,
                item_id,
                method,
                "pending",
                request_bytes,
                created_at.timestamp_millis()
            ],
        )?;
        Ok(StoredApproval {
            approval: PendingApproval {
                approval_token: token,
                thread_id,
                turn_id,
                item_id,
                method,
                state: ApprovalState::Pending,
                created_at,
                request,
            },
            upstream_epoch,
            upstream_request_id,
        })
    }

    pub fn mark_presented(&self, token: &str) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("approval registry poisoned");
        conn.execute(
            "UPDATE pending_server_requests
             SET state = 'presented', presented_at_ms = ?1
             WHERE local_token = ?2 AND state IN ('pending', 'presented')",
            params![now, token],
        )?;
        Ok(())
    }

    pub fn get(&self, token: &str) -> Result<Option<StoredApproval>, StoreError> {
        let conn = self.conn.lock().expect("approval registry poisoned");
        let row = conn
            .query_row(
                "SELECT local_token, upstream_epoch, upstream_request_id, upstream_request_id_json,
                        thread_id, turn_id, item_id, method, state, request_ciphertext, created_at_ms
                 FROM pending_server_requests
                 WHERE local_token = ?1",
                params![token],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Vec<u8>>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((
                token,
                epoch,
                request_id_text,
                request_id_json,
                thread_id,
                turn_id,
                item_id,
                method,
                state,
                request_bytes,
                created_ms,
            )) => {
                let upstream_request_id = if let Some(bytes) = request_id_json {
                    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                    JsonRpcId::from_value(&value).unwrap_or(JsonRpcId::String(request_id_text))
                } else if let Ok(n) = request_id_text.parse::<i64>() {
                    JsonRpcId::Number(n)
                } else {
                    JsonRpcId::String(request_id_text)
                };
                Some(StoredApproval {
                    approval: PendingApproval {
                        approval_token: token,
                        thread_id,
                        turn_id,
                        item_id,
                        method,
                        state: parse_state(&state),
                        created_at: chrono::DateTime::from_timestamp_millis(created_ms)
                            .unwrap_or_else(Utc::now),
                        request: serde_json::from_slice(&request_bytes).unwrap_or(Value::Null),
                    },
                    upstream_epoch: epoch as u64,
                    upstream_request_id,
                })
            }
            None => None,
        })
    }

    pub fn resolve(
        &self,
        token: &str,
        decision: ApprovalDecision,
        response: &Value,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let state = match decision {
            ApprovalDecision::Accept => "accepted",
            ApprovalDecision::Decline => "declined",
        };
        let response_bytes = serde_json::to_vec(response)?;
        let conn = self.conn.lock().expect("approval registry poisoned");
        conn.execute(
            "UPDATE pending_server_requests
             SET state = ?1, response_ciphertext = ?2, resolved_at_ms = ?3
             WHERE local_token = ?4",
            params![state, response_bytes, now, token],
        )?;
        Ok(())
    }

    pub fn orphan_epoch(&self, epoch: u64) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect("approval registry poisoned");
        let changed = conn.execute(
            "UPDATE pending_server_requests
             SET state = 'orphaned', resolved_at_ms = ?1
             WHERE upstream_epoch = ?2 AND state IN ('pending', 'presented')",
            params![Utc::now().timestamp_millis(), epoch as i64],
        )?;
        Ok(changed as u64)
    }

    pub fn list_pending_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingApproval>, StoreError> {
        let conn = self.conn.lock().expect("approval registry poisoned");
        let mut stmt = conn.prepare(
            "SELECT local_token, thread_id, turn_id, item_id, method, state, request_ciphertext, created_at_ms
             FROM pending_server_requests
             WHERE thread_id = ?1 AND state IN ('pending', 'presented')
             ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            let request_bytes: Vec<u8> = row.get(6)?;
            let created_ms: i64 = row.get(7)?;
            Ok(PendingApproval {
                approval_token: row.get(0)?,
                thread_id: row.get(1)?,
                turn_id: row.get(2)?,
                item_id: row.get(3)?,
                method: row.get(4)?,
                state: parse_state(&row.get::<_, String>(5)?),
                created_at: chrono::DateTime::from_timestamp_millis(created_ms)
                    .unwrap_or_else(Utc::now),
                request: serde_json::from_slice(&request_bytes).unwrap_or(Value::Null),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn parse_state(value: &str) -> ApprovalState {
    match value {
        "pending" => ApprovalState::Pending,
        "presented" => ApprovalState::Presented,
        "accepted" => ApprovalState::Accepted,
        "declined" => ApprovalState::Declined,
        "cancelled" => ApprovalState::Cancelled,
        "resolved" => ApprovalState::Resolved,
        "expired" => ApprovalState::Expired,
        _ => ApprovalState::Orphaned,
    }
}
