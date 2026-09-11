//! 歷史落盤與續傳（doc/05）—— 整個 App 最重要的一章。
//!
//! 第三方端點回 `store: false`，不記住上一輪。代理自己記：
//! 收到回應時把整段對話寫進 sqlite（加密），下次看到
//! `previous_response_id` 就把歷史攤回請求裡，再把那個欄位拿掉。
//!
//! 兩層儲存：記憶體（熱路徑）+ 磁碟（重開後接得上）。
//! 這裡只實作磁碟層 + 純函式；記憶體層與代理轉發的接線在後續章節。
//!
//! 純函式（`input_value_items`、`flatten_chain_items`、`hydrate_input`、
//! `estimate_items_tokens`）完全不碰磁碟，可單測。

use crate::crypto::JournalCipher;
use crate::error::{AppError, AppResult};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 約略 1 token ≈ 4 bytes（JSON 序列化後）。用於預算截斷的粗估。
pub const BYTES_PER_TOKEN: usize = 4;

/// 預設保留期（doc/05）。
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// 預設歷史 token 預算（128k × 95%，doc/05 的保底）。
pub const DEFAULT_HISTORY_TOKEN_BUDGET: usize = 121_600;
pub const MAX_HISTORY_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const KEEP_RECENT_SNAPSHOTS_PER_CONVERSATION: usize = 8;
const MAINTENANCE_EVERY_WRITES: usize = 32;
const ZSTD_MAGIC: &[u8; 4] = b"\x28\xb5\x2f\xfd";
const TELEMETRY_HISTORY_TRUNCATED_WITHOUT_COMPACTION: &str = "history_truncated_without_compaction";
const TELEMETRY_HISTORY_TRUNCATED_ITEMS: &str = "history_truncated_items";
const TELEMETRY_LAST_MAINTENANCE_AT: &str = "last_maintenance_at";
const TELEMETRY_LAST_VACUUM_AT: &str = "last_vacuum_at";
const TELEMETRY_LAST_JOURNAL_RETENTION_DELETED: &str = "last_journal_retention_deleted";

/// 落盤的解密後內容（input + output items）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub response_id: String,
    pub previous_response_id: Option<String>,
    /// 轉換前的原始 input items（doc/05 陷阱 1）。
    pub input_items: Vec<Value>,
    /// 該回應的 output items。
    pub output_items: Vec<Value>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySessionSnapshot {
    pub conversation_key: String,
    pub route_id: String,
    pub model: Option<String>,
    pub payload_bytes: u64,
    pub last_activity_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionJournalKind {
    Official,
    Local,
    #[default]
    LegacyRecovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionEngine {
    Canonical,
    Legacy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    #[default]
    Provisional,
    Verified,
    Failed,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionJournal {
    #[serde(default = "default_journal_schema_version")]
    pub schema_version: u32,
    #[serde(alias = "head_response_id")]
    pub compaction_id: String,
    #[serde(default)]
    pub kind: CompactionJournalKind,
    #[serde(default, alias = "original_items")]
    pub source_items: Vec<Value>,
    #[serde(default, alias = "replacement_items")]
    pub canonical_items: Vec<Value>,
    #[serde(default)]
    pub portable_items: Option<Vec<Value>>,
    pub created_at: i64,
    #[serde(default = "default_compaction_trigger")]
    pub trigger: String,
    #[serde(default)]
    pub producer_route: Option<String>,
    #[serde(default, alias = "provider")]
    pub producer_provider: Option<String>,
    #[serde(default, alias = "model")]
    pub producer_model: Option<String>,
    #[serde(default)]
    pub canonical_sha256: String,
    #[serde(default)]
    pub canonical_available: bool,
    /// v3: opaque account/realm fingerprint (never raw account IDs).
    #[serde(default)]
    pub realm_fingerprint: Option<String>,
    #[serde(default)]
    pub parent_compaction_id: Option<String>,
    /// Resolved engine that produced this record. Absent on rows persisted
    /// before the Codex Local Compact 0.150 switchover, which is exactly how
    /// a retired-engine record is recognised and refused.
    #[serde(default)]
    pub engine_id: Option<String>,
    #[serde(default)]
    pub engine_provenance: Option<String>,
    #[serde(default)]
    pub route_id: Option<String>,
    #[serde(default)]
    pub upstream_model: Option<String>,
    #[serde(default)]
    pub tokens_before: Option<u64>,
    #[serde(default)]
    pub tokens_after: Option<u64>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub generation: u32,
    #[serde(default)]
    pub status: CheckpointStatus,
    #[serde(default)]
    pub continuation_mode: Option<String>,
    /// Issue #4: `native` or `semantic` continuity for this checkpoint.
    #[serde(default)]
    pub continuity_kind: Option<String>,
    /// Issue #4: resolved compaction strategy label (provider_native / vellum_canonical / disabled).
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub reasoning_context_requested: Option<String>,
    #[serde(default)]
    pub reasoning_context_effective: Option<String>,
    #[serde(default)]
    pub contains_encrypted_reasoning: bool,
    #[serde(default)]
    pub reasoning_continuity_verified: bool,
    #[serde(default)]
    pub source_head_response_id: Option<String>,
    #[serde(default)]
    pub verified_followup_response_id: Option<String>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub dropped_message_count: Option<u64>,
    #[serde(default)]
    pub checkpoint_schema_version: Option<u32>,
    #[serde(default)]
    pub source_hash: Option<String>,
    #[serde(default)]
    pub checkpoint_hash: Option<String>,
    #[serde(default)]
    pub audit: Option<vellum_proxy_runtime::history::CanonicalAuditRecord>,
}

fn default_journal_schema_version() -> u32 {
    1
}

/// Current on-disk journal schema.
pub const COMPACTION_JOURNAL_SCHEMA_V3: u32 = 3;

/// Hash provider identity into a stable realm fingerprint without storing raw
/// account/team identifiers.
pub fn realm_fingerprint(
    provider_family: &str,
    base_url: &str,
    auth_method: &str,
    account_subject: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [provider_family, base_url, auth_method, account_subject] {
        hasher.update(part.trim().to_ascii_lowercase().as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn default_compaction_trigger() -> String {
    "manual".to_string()
}

fn canonical_items_sha256(items: &[Value]) -> AppResult<String> {
    let encoded = serde_json::to_vec(items)
        .map_err(|error| AppError::Message(format!("encode canonical journal items: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn normalize_compaction_journal(mut journal: CompactionJournal) -> AppResult<CompactionJournal> {
    if journal.schema_version < 2 {
        let was_official_placeholder = journal.trigger == "official";
        journal.kind = if was_official_placeholder || journal.trigger.starts_with("official") {
            CompactionJournalKind::LegacyRecovered
        } else {
            CompactionJournalKind::Local
        };
        journal.canonical_available =
            !was_official_placeholder && !journal.canonical_items.is_empty();
        if was_official_placeholder {
            // V1 official journals stored source/source and therefore did not
            // contain OpenAI's canonical compact output. Keep the source for
            // recovery, but never present the duplicate as canonical state.
            journal.canonical_items.clear();
        }
        journal.schema_version = 2;
    }
    if journal.schema_version < COMPACTION_JOURNAL_SCHEMA_V3 {
        // v3 adds lifecycle / continuity fields. Existing journals start as
        // provisional unless they already recorded a verified follow-up.
        if journal.verified_followup_response_id.is_some() {
            journal.status = CheckpointStatus::Verified;
            journal.reasoning_continuity_verified = true;
        } else if journal.status == CheckpointStatus::Provisional && journal.canonical_available {
            journal.status = CheckpointStatus::Provisional;
        }
        if journal.generation == 0 {
            journal.generation = 1;
        }
        journal.contains_encrypted_reasoning = journal_contains_encrypted_reasoning(&journal);
        journal.schema_version = COMPACTION_JOURNAL_SCHEMA_V3;
    }
    if journal.canonical_available {
        let actual = canonical_items_sha256(&journal.canonical_items)?;
        if journal.canonical_sha256.is_empty() {
            journal.canonical_sha256 = actual;
        } else if journal.canonical_sha256 != actual {
            return Err(AppError::Message(format!(
                "canonical compaction journal {} failed hash validation",
                journal.compaction_id
            )));
        }
    } else {
        journal.canonical_sha256.clear();
    }
    Ok(journal)
}

fn journal_contains_encrypted_reasoning(journal: &CompactionJournal) -> bool {
    journal
        .canonical_items
        .iter()
        .chain(journal.source_items.iter())
        .any(|item| {
            item.get("type").and_then(Value::as_str) == Some("reasoning")
                && item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
        })
}

#[derive(Debug, Serialize, Deserialize)]
struct PayloadBody {
    input_items: Vec<Value>,
    output_items: Vec<Value>,
}

/// Separated history storage telemetry (issue #1 DoD).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStorageTelemetry {
    /// Live pages in the main DB: `(page_count - freelist_count) * page_size`.
    pub logical_live_bytes: u64,
    /// On-disk size of `history.sqlite3` (or equivalent main DB path).
    pub main_db_physical_bytes: u64,
    /// On-disk size of the WAL file when present.
    pub wal_bytes: u64,
    /// Sum of compaction journal blob lengths currently stored.
    pub compaction_journal_bytes: u64,
    /// Times hydrate drained prior history without a compaction checkpoint.
    pub history_truncated_without_compaction: u64,
    /// Total prior history items dropped by emergency hydrate drains.
    pub history_truncated_items: u64,
    pub last_maintenance_at: Option<i64>,
    pub last_vacuum_at: Option<i64>,
    pub last_journal_retention_deleted: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HydrateResult {
    pub restored_items: usize,
    /// Items dropped from the oldest end of a hydrated chain because the
    /// token budget was exceeded without a compaction checkpoint.
    pub emergency_truncated_items: usize,
}

/// Result of walking a response ancestry under a token budget.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChainResult {
    pub entries: Vec<HistoryEntry>,
    /// True when older rows were omitted because the budget was exhausted.
    pub budget_exhausted: bool,
    /// Count of history rows skipped after the budget cut (approximate).
    pub omitted_entries: usize,
}

/// Optional continuity metadata stored with a compaction journal.
#[derive(Debug, Clone, Default)]
pub struct CompactionSaveMeta {
    pub engine_id: Option<String>,
    pub engine_provenance: Option<String>,
    pub route_id: Option<String>,
    pub upstream_model: Option<String>,
    pub tokens_before: Option<u64>,
    pub tokens_after: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub realm_fingerprint: Option<String>,
    pub parent_compaction_id: Option<String>,
    pub generation: u32,
    pub continuation_mode: Option<String>,
    /// Issue #4 continuity label (`native` / `semantic`).
    pub continuity_kind: Option<String>,
    /// Issue #4 strategy label for telemetry.
    pub strategy: Option<String>,
    pub reasoning_context_requested: Option<String>,
    pub reasoning_context_effective: Option<String>,
    pub source_head_response_id: Option<String>,
    pub checkpoint_schema_version: Option<u32>,
    pub source_hash: Option<String>,
    pub checkpoint_hash: Option<String>,
    pub audit: Option<vellum_proxy_runtime::history::CanonicalAuditRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport {
    pub journals_deleted: usize,
    pub history_evicted: usize,
    pub checkpoint_truncated: bool,
    pub vacuum_ran: bool,
    pub telemetry: HistoryStorageTelemetry,
}

/// 加密 sqlite 歷史庫。
///
/// Connection 用 `parking_lot::Mutex` 包住——rusqlite::Connection 不是
/// Sync，而 `parking_lot` 不會中毒（poison）：任何一次操作 panic 都不會讓
/// 之後所有執行緒的 `.lock()` 永久失敗。單一執行緒 panic 只會讓那一次呼叫
/// 失敗，鎖本身立刻可再用——panic 圍堵靠呼叫端的 `catch_history_panic`
/// （見檔案底部），不是靠鎖本身。
/// 路徑當參數傳入，測試用 temp dir。
pub struct HistoryStore {
    db_path: PathBuf,
    conn: Mutex<Connection>,
    cipher: JournalCipher,
    writes_since_maintenance: AtomicUsize,
    compaction_engine: AtomicU8,
    /// Process-local mirror of the durable counter (also persisted).
    emergency_truncations: AtomicU64,
}

/// Open (or create) `db_path`, apply pragmas, and ensure schema. Shared by
/// initial open and by [`HistoryStore::recover_connection`] so a reopen after
/// a corruption-shaped error gets exactly the same setup as a fresh start.
fn open_and_init_connection(db_path: &Path) -> AppResult<Connection> {
    let conn =
        Connection::open(db_path).map_err(|e| AppError::Message(format!("無法開啟歷史庫：{e}")))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| AppError::Message(format!("設定歷史 journal 失敗：{error}")))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| AppError::Message(format!("設定歷史同步模式失敗：{error}")))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS response_history (
            response_id           TEXT PRIMARY KEY,
            previous_response_id  TEXT,
            blob                  BLOB NOT NULL,
            payload_bytes         INTEGER NOT NULL,
            created_at            INTEGER NOT NULL,
            context_tokens        INTEGER
         );
         CREATE INDEX IF NOT EXISTS ix_history_created
           ON response_history(created_at);
         CREATE TABLE IF NOT EXISTS response_realm (
            response_id TEXT PRIMARY KEY,
            route_id    TEXT NOT NULL,
            model       TEXT
         );
         CREATE TABLE IF NOT EXISTS compaction_journal (
            head_response_id TEXT PRIMARY KEY,
            blob             BLOB NOT NULL,
            created_at       INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS history_telemetry (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS recovery_snapshot (
             conversation_key TEXT PRIMARY KEY,
             blob             BLOB NOT NULL,
             updated_at       INTEGER NOT NULL
         );
         ",
    )
    .map_err(|e| AppError::Message(format!("無法建立歷史資料表：{e}")))?;
    let _ = conn.execute(
        "ALTER TABLE response_history ADD COLUMN conversation_key TEXT",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE response_history ADD COLUMN context_tokens INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE response_realm ADD COLUMN model TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE response_realm ADD COLUMN realm_fingerprint TEXT",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE response_realm ADD COLUMN model_family TEXT",
        [],
    );
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS response_compaction (
            response_id    TEXT NOT NULL,
            compaction_id  TEXT NOT NULL,
            relation       TEXT NOT NULL DEFAULT 'produced',
            PRIMARY KEY (response_id, compaction_id)
         );
         CREATE INDEX IF NOT EXISTS ix_response_compaction_response
           ON response_compaction(response_id);
         CREATE INDEX IF NOT EXISTS ix_response_compaction_compaction
           ON response_compaction(compaction_id);",
    )
    .map_err(|error| AppError::Message(format!("無法建立 response_compaction 表：{error}")))?;
    // Existing rows predate consumed/produced boundary tracking and all
    // represented producer mappings.
    let _ = conn.execute(
        "ALTER TABLE response_compaction ADD COLUMN relation TEXT NOT NULL DEFAULT 'produced'",
        [],
    );
    conn.execute(
        "CREATE INDEX IF NOT EXISTS ix_history_conversation_created
           ON response_history(conversation_key, created_at)",
        [],
    )
    .map_err(|error| AppError::Message(format!("建立對話索引失敗：{error}")))?;
    Ok(conn)
}

impl HistoryStore {
    /// Load the durable recovery snapshot for a conversation.
    ///
    /// Recovery state is keyed by conversation, not by route, so it survives a
    /// restart and follows the conversation across a provider switch. A row
    /// that no longer decodes is treated as absent: the conversation starts
    /// without prior recovery rather than being blocked by damaged bookkeeping.
    pub fn recovery_snapshot_blob(&self, conversation_key: &str) -> AppResult<Option<Vec<u8>>> {
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare("SELECT blob FROM recovery_snapshot WHERE conversation_key = ?1")
            .map_err(|error| AppError::Message(format!("讀取 recovery_snapshot 失敗：{error}")))?;
        let mut rows = statement
            .query([conversation_key])
            .map_err(|error| AppError::Message(format!("讀取 recovery_snapshot 失敗：{error}")))?;
        match rows
            .next()
            .map_err(|error| AppError::Message(format!("讀取 recovery_snapshot 失敗：{error}")))?
        {
            Some(row) => Ok(Some(row.get::<_, Vec<u8>>(0).map_err(|error| {
                AppError::Message(format!("解析 recovery_snapshot 失敗：{error}"))
            })?)),
            None => Ok(None),
        }
    }

    /// Persist the durable recovery snapshot for a conversation.
    pub fn put_recovery_snapshot_blob(
        &self,
        conversation_key: &str,
        blob: &[u8],
        updated_at: i64,
    ) -> AppResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO recovery_snapshot (conversation_key, blob, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(conversation_key) DO UPDATE SET blob = ?2, updated_at = ?3",
            rusqlite::params![conversation_key, blob, updated_at],
        )
        .map_err(|error| AppError::Message(format!("寫入 recovery_snapshot 失敗：{error}")))?;
        Ok(())
    }

    /// 開（或建）一個歷史庫。`db_path` 是 sqlite 檔完整路徑；
    /// `key_root` 是金鑰檔目錄（JournalCipher 會在裡面建/讀 key file）。
    pub fn open(db_path: PathBuf, key_root: &Path) -> AppResult<Self> {
        let cipher = JournalCipher::load_or_create(key_root)?;
        Self::open_with_cipher(db_path, cipher)
    }

    /// 測試入口：顯式注入 cipher（不碰磁碟讀金鑰）。
    pub fn open_with_cipher(db_path: PathBuf, cipher: JournalCipher) -> AppResult<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::Message(format!("無法建立歷史目錄：{e}")))?;
        }
        let conn = open_and_init_connection(&db_path)?;
        let emergency_truncations =
            read_telemetry_counter(&conn, TELEMETRY_HISTORY_TRUNCATED_WITHOUT_COMPACTION)?;
        let store = Self {
            db_path,
            conn: Mutex::new(conn),
            cipher,
            writes_since_maintenance: AtomicUsize::new(0),
            compaction_engine: AtomicU8::new(0),
            emergency_truncations: AtomicU64::new(emergency_truncations),
        };
        store.evict_to_payload_budget(MAX_HISTORY_PAYLOAD_BYTES)?;
        Ok(store)
    }

    /// Recover from a rusqlite error that looks like the connection or the
    /// on-disk file itself is unhealthy (corruption, disk I/O failure, "not a
    /// database") — as opposed to e.g. a constraint violation, which says
    /// nothing about the connection's health. Closes the current connection,
    /// reopens `db_path` fresh, re-applies pragmas/schema, and runs
    /// `PRAGMA quick_check` before letting the store resume normal
    /// operation. Does not retry the operation that triggered the error —
    /// callers see the original error, but subsequent calls get a healthy
    /// connection instead of repeating the same failure forever.
    fn recover_connection(&self) -> AppResult<()> {
        log::warn!(
            "[History] attempting connection recovery for {}",
            self.db_path.display()
        );
        let mut guard = self.conn.lock();
        let fresh = open_and_init_connection(&self.db_path)?;
        let integrity: String = fresh
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|error| AppError::Message(format!("歷史庫完整性檢查失敗：{error}")))?;
        if integrity != "ok" {
            return Err(AppError::Message(format!(
                "history database failed integrity check after recovery: {integrity}"
            )));
        }
        *guard = fresh;
        log::warn!("[History] connection recovered and passed quick_check");
        Ok(())
    }

    /// True for rusqlite errors that indicate the connection/file is bad
    /// (corruption, I/O failure, wrong file format) rather than errors that
    /// just mean "the query/data was bad" (constraint violations, type
    /// mismatches, not-found). Only the former is worth reopening the
    /// connection over.
    fn is_corruption_shaped(error: &rusqlite::Error) -> bool {
        match error {
            rusqlite::Error::SqliteFailure(ffi_error, _) => matches!(
                ffi_error.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::SystemIoFailure
            ),
            _ => false,
        }
    }

    /// Run `op` with the connection lock held; on a corruption-shaped
    /// rusqlite error, best-effort recover the connection (log-only on
    /// failure — the original error is still what's returned to the caller)
    /// so the *next* call isn't stuck against the same broken connection.
    fn with_conn_recovering<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, rusqlite::Error> {
        let result = {
            let guard = self.conn.lock();
            op(&guard)
        };
        if let Err(error) = &result {
            if Self::is_corruption_shaped(error) {
                if let Err(recover_error) = self.recover_connection() {
                    log::error!("[History] connection recovery failed: {recover_error}");
                }
            }
        }
        result
    }

    pub fn compaction_engine(&self) -> CompactionEngine {
        if self.compaction_engine.load(Ordering::Relaxed) == 1 {
            CompactionEngine::Legacy
        } else {
            CompactionEngine::Canonical
        }
    }

    /// Legacy compaction is deliberately available only to the isolated eval
    /// runner. Desktop-created stores always remain canonical.
    pub(crate) fn set_eval_compaction_engine(&self, engine: CompactionEngine) {
        self.compaction_engine.store(
            if engine == CompactionEngine::Legacy {
                1
            } else {
                0
            },
            Ordering::Relaxed,
        );
    }

    /// Record an emergency hydrate drain that dropped prior history without a
    /// compaction checkpoint. Counters are durable across restarts.
    pub fn record_emergency_history_truncation(&self, truncated_items: usize) -> AppResult<()> {
        if truncated_items == 0 {
            return Ok(());
        }
        self.emergency_truncations.fetch_add(1, Ordering::Relaxed);
        let conn = self.conn.lock();
        bump_telemetry_counter(&conn, TELEMETRY_HISTORY_TRUNCATED_WITHOUT_COMPACTION, 1)?;
        bump_telemetry_counter(
            &conn,
            TELEMETRY_HISTORY_TRUNCATED_ITEMS,
            truncated_items as u64,
        )?;
        log::warn!(
            "[History] emergency truncation without compaction dropped {truncated_items} item(s)"
        );
        Ok(())
    }

    pub fn history_truncated_without_compaction(&self) -> u64 {
        self.emergency_truncations.load(Ordering::Relaxed)
    }

    /// Separated storage metrics: logical live pages, main DB physical size,
    /// WAL size, and compaction journal blob bytes.
    pub fn storage_telemetry(&self) -> AppResult<HistoryStorageTelemetry> {
        let conn = self.conn.lock();
        let page_count = pragma_u64(&conn, "page_count")?;
        let free_pages = pragma_u64(&conn, "freelist_count")?;
        let page_size = pragma_u64(&conn, "page_size")?;
        let logical_live_bytes = page_count
            .saturating_sub(free_pages)
            .saturating_mul(page_size);
        let main_db_physical_bytes = std::fs::metadata(&self.db_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let mut wal_os = self.db_path.as_os_str().to_os_string();
        wal_os.push("-wal");
        let wal_bytes = std::fs::metadata(PathBuf::from(wal_os))
            .map(|meta| meta.len())
            .unwrap_or(0);
        let compaction_journal_bytes: u64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(blob)), 0) FROM compaction_journal",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                AppError::Message(format!("讀取 compaction journal 容量失敗：{error}"))
            })?;
        Ok(HistoryStorageTelemetry {
            logical_live_bytes,
            main_db_physical_bytes,
            wal_bytes,
            compaction_journal_bytes,
            history_truncated_without_compaction: read_telemetry_counter(
                &conn,
                TELEMETRY_HISTORY_TRUNCATED_WITHOUT_COMPACTION,
            )?,
            history_truncated_items: read_telemetry_counter(
                &conn,
                TELEMETRY_HISTORY_TRUNCATED_ITEMS,
            )?,
            last_maintenance_at: read_telemetry_counter_opt(&conn, TELEMETRY_LAST_MAINTENANCE_AT)?,
            last_vacuum_at: read_telemetry_counter_opt(&conn, TELEMETRY_LAST_VACUUM_AT)?,
            last_journal_retention_deleted: read_telemetry_counter(
                &conn,
                TELEMETRY_LAST_JOURNAL_RETENTION_DELETED,
            )?,
        })
    }

    /// Delete stale compaction journals with reference-aware retention.
    ///
    /// Keep:
    /// - provisional journals younger than the retention window (or still mapped);
    /// - any journal still referenced by `response_compaction` or live history;
    /// - parent of a kept child generation;
    /// - the newest verified journal overall.
    ///
    /// Age out unreferenced provisional journals past the cutoff as Failed then
    /// delete.
    pub fn evict_stale_compaction_journals(&self, retention_days: u32) -> AppResult<usize> {
        let retention_days = retention_days.max(1) as i64;
        let cutoff = now_unix_secs().saturating_sub(retention_days.saturating_mul(86_400));
        let mut referenced = HashSet::new();
        let mut journals = Vec::new();
        {
            let conn = self.conn.lock();
            let mut mapped = conn
                .prepare("SELECT DISTINCT compaction_id FROM response_compaction")
                .map_err(|error| {
                    AppError::Message(format!("讀取 response_compaction 失敗：{error}"))
                })?;
            for row in mapped
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| {
                    AppError::Message(format!("掃描 response_compaction 失敗：{error}"))
                })?
            {
                referenced.insert(row.map_err(|error| {
                    AppError::Message(format!("解析 response_compaction 失敗：{error}"))
                })?);
            }
            let mut statement = conn
                .prepare(
                    "SELECT head_response_id, blob, created_at
                     FROM compaction_journal
                     ORDER BY created_at DESC, rowid DESC",
                )
                .map_err(|error| {
                    AppError::Message(format!("讀取 compaction journal 清單失敗：{error}"))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|error| {
                    AppError::Message(format!("掃描 compaction journal 失敗：{error}"))
                })?;
            for row in rows {
                let (id, blob, created_at) = row.map_err(|error| {
                    AppError::Message(format!("讀取 compaction journal 列失敗：{error}"))
                })?;
                let aad = format!("compact:{id}");
                let plaintext = match self
                    .cipher
                    .open(&blob, aad.as_bytes())
                    .and_then(decode_payload)
                {
                    Ok(value) => value,
                    Err(error) => {
                        log::warn!("[History] stale journal {id} undecryptable: {error}");
                        if created_at < cutoff && !referenced.contains(&id) {
                            journals.push((id, created_at, CheckpointStatus::Failed, None, true));
                        }
                        continue;
                    }
                };
                let journal: CompactionJournal = match serde_json::from_slice(&plaintext) {
                    Ok(value) => normalize_compaction_journal(value)?,
                    Err(error) => {
                        log::warn!("[History] stale journal {id} undecodable: {error}");
                        if created_at < cutoff && !referenced.contains(&id) {
                            journals.push((id, created_at, CheckpointStatus::Failed, None, true));
                        }
                        continue;
                    }
                };
                journals.push((
                    id,
                    created_at,
                    journal.status,
                    journal.parent_compaction_id.clone(),
                    false,
                ));
            }
        }

        let mut keep = HashSet::new();
        // Always keep referenced mappings.
        for id in &referenced {
            keep.insert(id.clone());
        }
        // Keep newest verified globally.
        if let Some((id, _, _status, _, _)) = journals
            .iter()
            .find(|(_, _, status, _, corrupt)| !*corrupt && *status == CheckpointStatus::Verified)
        {
            keep.insert(id.clone());
        }
        // Keep young provisional / any parent of a kept child.
        for (id, created_at, status, parent, corrupt) in &journals {
            if *corrupt {
                continue;
            }
            if *status == CheckpointStatus::Provisional && *created_at >= cutoff {
                keep.insert(id.clone());
            }
            if keep.contains(id) {
                if let Some(parent) = parent {
                    keep.insert(parent.clone());
                }
            }
        }
        // Second pass: parents of newly kept children.
        for (id, _, _, parent, _) in &journals {
            if keep.contains(id) {
                if let Some(parent) = parent {
                    keep.insert(parent.clone());
                }
            }
        }

        let mut candidates = Vec::new();
        for (id, created_at, status, _, corrupt) in &journals {
            if keep.contains(id) {
                continue;
            }
            if *corrupt || *created_at < cutoff {
                // Age-out unreferenced provisional as well as superseded/failed.
                if *corrupt
                    || matches!(
                        status,
                        CheckpointStatus::Superseded
                            | CheckpointStatus::Failed
                            | CheckpointStatus::Provisional
                            | CheckpointStatus::Verified
                    )
                {
                    candidates.push(id.clone());
                }
            }
        }

        if candidates.is_empty() {
            let _ = self.cleanup_dangling_response_compaction()?;
            return Ok(0);
        }
        // Batch delete + telemetry counter as one unit — a crash mid-batch
        // must not leave the retention counter ahead of what was actually
        // deleted, and must not leave a journal deleted without its mapping
        // row (or vice versa).
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Message(format!("開啟保留期交易失敗：{error}")))?;
        let mut deleted = 0usize;
        for id in &candidates {
            deleted += tx
                .execute(
                    "DELETE FROM compaction_journal WHERE head_response_id = ?1",
                    params![id],
                )
                .map_err(|error| {
                    AppError::Message(format!("刪除過期 compaction journal 失敗：{error}"))
                })?;
            let _ = tx.execute(
                "DELETE FROM response_compaction WHERE compaction_id = ?1",
                params![id],
            );
        }
        set_telemetry_counter(
            &tx,
            TELEMETRY_LAST_JOURNAL_RETENTION_DELETED,
            deleted as u64,
        )?;
        tx.commit()
            .map_err(|error| AppError::Message(format!("提交保留期交易失敗：{error}")))?;
        drop(conn);
        let _ = self.cleanup_dangling_response_compaction()?;
        Ok(deleted)
    }

    /// Remove response_compaction rows whose response or journal no longer exists.
    pub fn cleanup_dangling_response_compaction(&self) -> AppResult<usize> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM response_compaction
                 WHERE response_id NOT IN (SELECT response_id FROM response_history)
                    OR compaction_id NOT IN (SELECT head_response_id FROM compaction_journal)",
                [],
            )
            .map_err(|error| {
                AppError::Message(format!("清理 dangling response_compaction 失敗：{error}"))
            })?;
        Ok(deleted)
    }

    /// Stopped-state maintenance: age-based history eviction, journal
    /// retention, WAL checkpoint, and optional VACUUM when free pages are high.
    /// Call only when the proxy is stopped so writers are quiescent.
    pub fn run_stopped_state_maintenance(
        &self,
        retention_days: u32,
    ) -> AppResult<MaintenanceReport> {
        let history_evicted = self.evict_older_than_days(retention_days)?;
        let journals_deleted = self.evict_stale_compaction_journals(retention_days)?;
        let _ = self.evict_to_payload_budget(MAX_HISTORY_PAYLOAD_BYTES)?;

        let mut checkpoint_truncated = false;
        let mut vacuum_ran = false;
        {
            let conn = self.conn.lock();
            // Checkpoint WAL into the main DB and truncate the WAL file.
            match conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
                Ok(()) => checkpoint_truncated = true,
                Err(error) => {
                    // Some rusqlite builds expose checkpoint only via query_row.
                    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
                        Ok(()) => checkpoint_truncated = true,
                        Err(inner) => log::warn!(
                            "[History] wal_checkpoint failed: {error}; fallback: {inner}"
                        ),
                    }
                }
            }
            let page_count = pragma_u64(&conn, "page_count").unwrap_or(0);
            let free_pages = pragma_u64(&conn, "freelist_count").unwrap_or(0);
            let free_ratio = if page_count == 0 {
                0.0
            } else {
                free_pages as f64 / page_count as f64
            };
            // VACUUM only when enough free pages justify the rewrite cost.
            if free_ratio >= 0.25 && free_pages >= 64 {
                match conn.execute_batch("VACUUM;") {
                    Ok(()) => {
                        vacuum_ran = true;
                        set_telemetry_counter(
                            &conn,
                            TELEMETRY_LAST_VACUUM_AT,
                            now_unix_secs() as u64,
                        )?;
                    }
                    Err(error) => log::warn!("[History] VACUUM failed: {error}"),
                }
            }
            set_telemetry_counter(&conn, TELEMETRY_LAST_MAINTENANCE_AT, now_unix_secs() as u64)?;
        }

        Ok(MaintenanceReport {
            journals_deleted,
            history_evicted,
            checkpoint_truncated,
            vacuum_ran,
            telemetry: self.storage_telemetry()?,
        })
    }

    /// 記一輪往返。`response` 的 `id` 當主鍵；`request` 的
    /// `previous_response_id` 當鏈結；input/output items 加密後落盤。
    /// 回傳 true 代表新插入／更新成功。
    pub fn record_exchange(&self, request: &Value, response: &Value) -> AppResult<bool> {
        self.record_exchange_with_conversation_key(request, response, None)
    }

    /// Like [`record_exchange`], but an explicit conversation key (resolved
    /// session identity) overrides re-derivation from the request body so
    /// reconnect with a changed `prompt_cache_key` stays one conversation
    /// (issue #4 comment 5146162101 B-2).
    pub fn record_exchange_with_conversation_key(
        &self,
        request: &Value,
        response: &Value,
        conversation_key_override: Option<&str>,
    ) -> AppResult<bool> {
        let Some(response_id) = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
        else {
            // 沒有 id 就不記——攤回時找不到。
            return Ok(false);
        };

        let previous_response_id = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let input_items = request_input_items(request);
        let output_items = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let body = PayloadBody {
            input_items,
            output_items,
        };
        let plaintext = serde_json::to_vec(&body)
            .map_err(|e| AppError::Message(format!("歷史內容序列化失敗：{e}")))?;
        let payload_bytes = plaintext.len() as i64;
        let encoded = encode_payload(&plaintext)?;
        let blob = self.cipher.seal(&encoded, response_id.as_bytes())?;
        let created_at = now_unix_secs();
        let conversation_key = conversation_key_override
            .filter(|v| !v.trim().is_empty())
            .map(str::to_string)
            .or_else(|| conversation_key(request));
        let context_tokens = response_context_tokens(response);

        // INSERT + snapshot pruning must succeed or fail together — wrap in
        // an explicit transaction so a crash/panic between the two never
        // leaves a written row with a stale snapshot count, or vice versa.
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Message(format!("開啟歷史交易失敗：{e}")))?;
        if let Err(error) = tx.execute(
            "INSERT INTO response_history
               (response_id, previous_response_id, blob, payload_bytes, created_at,
                conversation_key, context_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(response_id) DO UPDATE SET
               previous_response_id = excluded.previous_response_id,
               blob = excluded.blob,
               payload_bytes = excluded.payload_bytes,
               created_at = excluded.created_at,
               conversation_key = excluded.conversation_key,
               context_tokens = excluded.context_tokens",
            params![
                response_id,
                previous_response_id,
                blob,
                payload_bytes,
                created_at,
                conversation_key,
                context_tokens,
            ],
        ) {
            let corrupt = Self::is_corruption_shaped(&error);
            drop(tx);
            drop(conn);
            if corrupt {
                if let Err(recover_error) = self.recover_connection() {
                    log::error!("[History] connection recovery failed: {recover_error}");
                }
            }
            return Err(AppError::Message(format!("寫入歷史失敗：{error}")));
        }
        if let Some(key) = conversation_key.as_deref() {
            prune_conversation_snapshots(&tx, key, KEEP_RECENT_SNAPSHOTS_PER_CONVERSATION)?;
        }
        tx.commit()
            .map_err(|e| AppError::Message(format!("提交歷史交易失敗：{e}")))?;
        drop(conn);
        if self
            .writes_since_maintenance
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            >= MAINTENANCE_EVERY_WRITES
        {
            self.writes_since_maintenance.store(0, Ordering::Relaxed);
            self.evict_to_payload_budget(MAX_HISTORY_PAYLOAD_BYTES)?;
        }
        Ok(true)
    }

    pub fn mark_response_route(&self, response_id: &str, route_id: &str) -> AppResult<()> {
        self.mark_response_context(response_id, route_id, None)
    }

    pub fn mark_response_context(
        &self,
        response_id: &str,
        route_id: &str,
        model: Option<&str>,
    ) -> AppResult<()> {
        self.mark_response_context_full(response_id, route_id, model, None, None)
    }

    pub fn mark_response_context_full(
        &self,
        response_id: &str,
        route_id: &str,
        model: Option<&str>,
        realm_fingerprint: Option<&str>,
        model_family: Option<&str>,
    ) -> AppResult<()> {
        self.conn
            .lock()
            .execute(
                "INSERT INTO response_realm
                   (response_id, route_id, model, realm_fingerprint, model_family)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(response_id) DO UPDATE SET
                   route_id = excluded.route_id,
                   model = COALESCE(excluded.model, response_realm.model),
                   realm_fingerprint = COALESCE(excluded.realm_fingerprint, response_realm.realm_fingerprint),
                   model_family = COALESCE(excluded.model_family, response_realm.model_family)",
                params![
                    response_id,
                    route_id,
                    model,
                    realm_fingerprint,
                    model_family
                ],
            )
            .map_err(|error| AppError::Message(format!("寫入回應 realm 失敗：{error}")))?;
        Ok(())
    }

    pub fn response_realm_fingerprint(&self, response_id: &str) -> AppResult<Option<String>> {
        self.conn
            .lock()
            .query_row(
                "SELECT realm_fingerprint FROM response_realm WHERE response_id = ?1",
                [response_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(|error| AppError::Message(format!("讀取 realm fingerprint 失敗：{error}")))
    }

    pub fn response_model(&self, response_id: &str) -> AppResult<Option<String>> {
        self.conn
            .lock()
            .query_row(
                "SELECT model FROM response_realm WHERE response_id = ?1",
                [response_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(|error| AppError::Message(format!("讀取 response model 失敗：{error}")))
    }

    pub fn bind_response_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
    ) -> AppResult<()> {
        self.conn
            .lock()
            .execute(
                "INSERT INTO response_compaction (response_id, compaction_id, relation)
                 VALUES (?1, ?2, 'produced')
                 ON CONFLICT(response_id, compaction_id) DO UPDATE SET relation = 'produced'",
                params![response_id, compaction_id],
            )
            .map_err(|error| {
                AppError::Message(format!("寫入 response_compaction 對應失敗：{error}"))
            })?;
        Ok(())
    }

    /// Bind a checkpoint that was consumed while producing `response_id`.
    ///
    /// The durable mapping table is shared with produced checkpoints, while
    /// the `relation` column preserves which side of the response boundary the
    /// checkpoint belongs to. This is essential when a later request only
    /// supplies `previous_response_id`.
    pub fn bind_consumed_response_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
    ) -> AppResult<()> {
        self.conn
            .lock()
            .execute(
                "INSERT INTO response_compaction (response_id, compaction_id, relation)
                 VALUES (?1, ?2, 'consumed')
                 ON CONFLICT(response_id, compaction_id) DO UPDATE SET relation = 'consumed'",
                params![response_id, compaction_id],
            )
            .map_err(|error| {
                AppError::Message(format!(
                    "cannot bind consumed response compaction boundary: {error}"
                ))
            })?;
        Ok(())
    }

    fn compaction_bindings_for_response(
        &self,
        response_id: &str,
    ) -> AppResult<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT compaction_id, relation
                   FROM response_compaction WHERE response_id = ?1",
            )
            .map_err(|error| {
                AppError::Message(format!("query response compaction bindings: {error}"))
            })?;
        let rows = statement
            .query_map([response_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| {
                AppError::Message(format!("read response compaction bindings: {error}"))
            })?;
        let mut bindings = Vec::new();
        for row in rows {
            bindings.push(row.map_err(|error| {
                AppError::Message(format!("decode response compaction binding: {error}"))
            })?);
        }
        Ok(bindings)
    }

    pub fn compaction_ids_for_response(&self, response_id: &str) -> AppResult<Vec<String>> {
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare("SELECT compaction_id FROM response_compaction WHERE response_id = ?1")
            .map_err(|error| {
                AppError::Message(format!("查詢 response_compaction 失敗：{error}"))
            })?;
        let rows = statement
            .query_map([response_id], |row| row.get(0))
            .map_err(|error| {
                AppError::Message(format!("讀取 response_compaction 失敗：{error}"))
            })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|error| {
                AppError::Message(format!("解析 response_compaction 失敗：{error}"))
            })?);
        }
        Ok(ids)
    }

    /// Resolve the provider realm that most recently completed a response for
    /// the same Codex conversation. Codex Desktop's WebSocket transport often
    /// sends a full input snapshot without `previous_response_id`, especially
    /// immediately after a model switch, so response-chain lookup alone is
    /// insufficient to decide whether encrypted reasoning can be replayed.
    pub fn latest_conversation_route(&self, request: &Value) -> AppResult<Option<String>> {
        let Some(key) = conversation_key(request) else {
            return Ok(None);
        };
        self.conn
            .lock()
            .query_row(
                "SELECT realm.route_id
                   FROM response_history history
                   JOIN response_realm realm
                     ON realm.response_id = history.response_id
                  WHERE history.conversation_key = ?1
                  ORDER BY history.created_at DESC, history.rowid DESC
                  LIMIT 1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                AppError::Message(format!(
                    "cannot resolve latest conversation provider realm: {error}"
                ))
            })
    }

    /// 每個 conversation 只回一列。優先使用最新一輪的上游 usage；
    /// 舊資料沒有 usage 時只估最新快照，不能把多份完整快照相加。
    /// 最新一輪同時決定 Provider、模型與最後活動時間。
    pub fn session_snapshots(&self) -> AppResult<Vec<HistorySessionSnapshot>> {
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT grouped.conversation_key,
                        COALESCE(realm.route_id, ''),
                        realm.model,
                        CASE
                          WHEN grouped.context_tokens > 0
                            THEN grouped.context_tokens * ?1
                          ELSE grouped.latest_payload_bytes
                        END,
                        grouped.last_activity_at
                 FROM (
                   SELECT conversation_key,
                          (
                            SELECT latest.payload_bytes
                            FROM response_history latest
                            WHERE latest.conversation_key = response_history.conversation_key
                            ORDER BY latest.created_at DESC, latest.rowid DESC
                            LIMIT 1
                          ) AS latest_payload_bytes,
                          MAX(created_at) AS last_activity_at,
                          (
                            SELECT latest.response_id
                            FROM response_history latest
                            WHERE latest.conversation_key = response_history.conversation_key
                            ORDER BY latest.created_at DESC, latest.rowid DESC
                            LIMIT 1
                          ) AS latest_response_id,
                          (
                            SELECT COALESCE(latest.context_tokens, 0)
                            FROM response_history latest
                            WHERE latest.conversation_key = response_history.conversation_key
                            ORDER BY latest.created_at DESC, latest.rowid DESC
                            LIMIT 1
                          ) AS context_tokens
                   FROM response_history
                   WHERE conversation_key IS NOT NULL
                     AND TRIM(conversation_key) <> ''
                   GROUP BY conversation_key
                 ) grouped
                 LEFT JOIN response_realm realm
                   ON realm.response_id = grouped.latest_response_id
                 ORDER BY grouped.last_activity_at DESC",
            )
            .map_err(|error| AppError::Message(format!("準備工作階段查詢失敗：{error}")))?;
        let rows = statement
            .query_map([BYTES_PER_TOKEN as i64], |row| {
                let payload_bytes: i64 = row.get(3)?;
                Ok(HistorySessionSnapshot {
                    conversation_key: row.get(0)?,
                    route_id: row.get(1)?,
                    model: row.get(2)?,
                    payload_bytes: payload_bytes.max(0) as u64,
                    last_activity_at: row.get(4)?,
                })
            })
            .map_err(|error| AppError::Message(format!("查詢工作階段失敗：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("讀取工作階段失敗：{error}")))
    }

    pub fn response_route(&self, response_id: &str) -> AppResult<Option<String>> {
        self.conn
            .lock()
            .query_row(
                "SELECT route_id FROM response_realm WHERE response_id = ?1",
                [response_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| AppError::Message(format!("讀取回應 realm 失敗：{error}")))
    }

    /// 取單筆（解密）。
    pub fn get(&self, response_id: &str) -> AppResult<Option<HistoryEntry>> {
        let row: Option<(Option<String>, Vec<u8>, i64)> = self
            .with_conn_recovering(|conn| {
                conn.query_row(
                    "SELECT previous_response_id, blob, created_at
                     FROM response_history WHERE response_id = ?1",
                    [response_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
            })
            .map_err(|e| AppError::Message(format!("讀取歷史失敗：{e}")))?;
        let Some((previous_response_id, blob, created_at)) = row else {
            return Ok(None);
        };
        match self.open_blob(response_id, &blob, previous_response_id, created_at) {
            Ok(entry) => Ok(Some(entry)),
            Err(e) => {
                log::warn!("[History] 無法解密 {response_id}：{e}");
                Ok(None)
            }
        }
    }

    /// 沿 `previous_response_id` 鏈往回走，累積到超過 token 預算就停。
    /// 回傳 oldest → newest。**必須擋迴圈**（doc/05 陷阱）。
    pub fn get_chain(&self, head: &str, token_budget: usize) -> AppResult<Vec<HistoryEntry>> {
        Ok(self.get_chain_detailed(head, token_budget)?.entries)
    }

    /// Like [`get_chain`] but reports whether older history was omitted by the
    /// token budget (silent truncation without compaction).
    pub fn get_chain_detailed(&self, head: &str, token_budget: usize) -> AppResult<ChainResult> {
        let byte_budget = token_budget.saturating_mul(BYTES_PER_TOKEN).max(1);
        let conn = self.conn.lock();
        let mut chain_newest_first = Vec::new();
        let mut current = Some(head.to_string());
        let mut seen = HashSet::new();
        let mut total_bytes = 0usize;
        let mut budget_exhausted = false;
        let mut omitted_entries = 0usize;

        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break; // 迴圈防護
            }
            let row: Option<(Option<String>, Vec<u8>, i64)> = conn
                .query_row(
                    "SELECT previous_response_id, blob, created_at
                     FROM response_history WHERE response_id = ?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| AppError::Message(format!("讀取歷史鏈失敗：{e}")))?;
            let Some((prev, blob, created_at)) = row else {
                break;
            };
            // 先估這筆大小；超過預算就停（但第一筆一定要取，否則空鏈）。
            let plaintext = match self
                .cipher
                .open(&blob, id.as_bytes())
                .and_then(decode_payload)
            {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("[History] 鏈中 {id} 解密失敗，跳過：{e}");
                    break;
                }
            };
            let row_bytes = plaintext.len();
            if total_bytes > 0 && total_bytes.saturating_add(row_bytes) > byte_budget {
                budget_exhausted = true;
                // Count remaining ancestry length for telemetry (capped walk).
                let mut skip = Some(id);
                let mut skip_seen = HashSet::new();
                while let Some(skip_id) = skip {
                    if !skip_seen.insert(skip_id.clone()) {
                        break;
                    }
                    omitted_entries = omitted_entries.saturating_add(1);
                    skip = conn
                        .query_row(
                            "SELECT previous_response_id FROM response_history WHERE response_id = ?1",
                            [&skip_id],
                            |row| row.get::<_, Option<String>>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                        .flatten();
                    if omitted_entries > 10_000 {
                        break;
                    }
                }
                break;
            }
            let body: PayloadBody = serde_json::from_slice(&plaintext)
                .map_err(|e| AppError::Message(format!("歷史內容解碼失敗：{e}")))?;
            total_bytes = total_bytes.saturating_add(row_bytes);
            current = prev.clone();
            chain_newest_first.push(HistoryEntry {
                response_id: id,
                previous_response_id: prev,
                input_items: body.input_items,
                output_items: body.output_items,
                created_at,
            });
        }

        chain_newest_first.reverse();
        Ok(ChainResult {
            entries: chain_newest_first,
            budget_exhausted,
            omitted_entries,
        })
    }

    /// 清掉超過保留期的資料。回傳刪除筆數。
    pub fn evict_older_than_days(&self, days: u32) -> AppResult<usize> {
        let days = days.max(1) as i64;
        let cutoff = now_unix_secs().saturating_sub(days.saturating_mul(86_400));
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM response_history WHERE created_at < ?1",
                params![cutoff],
            )
            .map_err(|e| AppError::Message(format!("清理歷史失敗：{e}")))?;
        conn.execute(
            "DELETE FROM response_realm
             WHERE response_id NOT IN (SELECT response_id FROM response_history)",
            [],
        )
        .map_err(|error| AppError::Message(format!("清理回應 realm 失敗：{error}")))?;
        Ok(deleted)
    }

    /// Keep live history pages under a bounded budget. Cleanup is deliberately
    /// incremental so opening Vellum never performs a multi-gigabyte VACUUM.
    /// Freed SQLite pages are reused by future writes; physical compaction can
    /// be performed separately while the proxy is stopped.
    pub fn evict_to_payload_budget(&self, max_bytes: u64) -> AppResult<usize> {
        let mut total_deleted = 0usize;
        // Loop until under budget or a pass deletes nothing (hard bound intent).
        for _ in 0..32 {
            let mut conn = self.conn.lock();
            let page_count = pragma_u64(&conn, "page_count")?;
            let free_pages = pragma_u64(&conn, "freelist_count")?;
            let page_size = pragma_u64(&conn, "page_size")?;
            let live_bytes = page_count
                .saturating_sub(free_pages)
                .saturating_mul(page_size);
            if live_bytes <= max_bytes {
                drop(conn);
                let _ = self.cleanup_dangling_response_compaction()?;
                return Ok(total_deleted);
            }
            // Both deletes are one eviction unit — response_realm must never
            // end up referencing a response_history row that a crash left
            // half-deleted.
            let tx = conn
                .transaction()
                .map_err(|error| AppError::Message(format!("開啟清理交易失敗：{error}")))?;
            let deleted = tx
                .execute(
                    "DELETE FROM response_history
                     WHERE response_id IN (
                       SELECT response_id
                       FROM response_history
                       WHERE response_id NOT IN (
                         SELECT response_id
                         FROM response_history
                         ORDER BY created_at DESC, rowid DESC
                         LIMIT 8
                       )
                       AND response_id NOT IN (
                         SELECT previous_response_id
                         FROM response_history
                         WHERE previous_response_id IS NOT NULL
                       )
                       AND response_id NOT IN (
                         SELECT response_id FROM response_compaction
                       )
                       AND response_id NOT IN (
                         SELECT source_head FROM (
                           SELECT head_response_id AS source_head FROM compaction_journal
                         )
                       )
                       ORDER BY created_at ASC, rowid ASC
                       LIMIT 256
                     )",
                    [],
                )
                .map_err(|error| AppError::Message(format!("清理歷史容量失敗：{error}")))?;
            tx.execute(
                "DELETE FROM response_realm
                 WHERE response_id NOT IN (SELECT response_id FROM response_history)",
                [],
            )
            .map_err(|error| AppError::Message(format!("清理回應 realm 失敗：{error}")))?;
            tx.commit()
                .map_err(|error| AppError::Message(format!("提交清理交易失敗：{error}")))?;
            drop(conn);
            let _ = self.cleanup_dangling_response_compaction()?;
            total_deleted = total_deleted.saturating_add(deleted);
            if deleted == 0 {
                break;
            }
        }
        Ok(total_deleted)
    }

    pub fn latest_response_id(&self) -> AppResult<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT response_id FROM response_history ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| AppError::Message(format!("讀取最新歷史失敗：{error}")))
    }

    pub fn save_compaction(
        &self,
        head_response_id: &str,
        original_items: Vec<Value>,
        replacement_items: Vec<Value>,
    ) -> AppResult<()> {
        self.save_compaction_with_metadata(
            head_response_id,
            original_items,
            replacement_items,
            "manual",
            None,
            None,
        )
    }

    pub fn save_compaction_with_metadata(
        &self,
        head_response_id: &str,
        original_items: Vec<Value>,
        replacement_items: Vec<Value>,
        trigger: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> AppResult<()> {
        self.save_canonical_compaction(
            head_response_id,
            CompactionJournalKind::Local,
            original_items,
            replacement_items,
            None,
            trigger,
            None,
            provider,
            model,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_canonical_compaction(
        &self,
        compaction_id: &str,
        kind: CompactionJournalKind,
        source_items: Vec<Value>,
        canonical_items: Vec<Value>,
        portable_items: Option<Vec<Value>>,
        trigger: &str,
        producer_route: Option<&str>,
        producer_provider: Option<&str>,
        producer_model: Option<&str>,
    ) -> AppResult<()> {
        self.save_canonical_compaction_with_meta(
            compaction_id,
            kind,
            source_items,
            canonical_items,
            portable_items,
            trigger,
            producer_route,
            producer_provider,
            producer_model,
            CompactionSaveMeta::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_canonical_compaction_with_meta(
        &self,
        compaction_id: &str,
        kind: CompactionJournalKind,
        source_items: Vec<Value>,
        canonical_items: Vec<Value>,
        portable_items: Option<Vec<Value>>,
        trigger: &str,
        producer_route: Option<&str>,
        producer_provider: Option<&str>,
        producer_model: Option<&str>,
        meta: CompactionSaveMeta,
    ) -> AppResult<()> {
        if canonical_items.is_empty() {
            return Err(AppError::Message(format!(
                "canonical compaction journal {compaction_id} cannot be empty"
            )));
        }
        let contains_encrypted_reasoning =
            source_items
                .iter()
                .chain(canonical_items.iter())
                .any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("reasoning")
                        && item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                });
        let generation = if meta.generation == 0 {
            1
        } else {
            meta.generation
        };
        let journal = CompactionJournal {
            schema_version: COMPACTION_JOURNAL_SCHEMA_V3,
            compaction_id: compaction_id.to_string(),
            engine_id: meta
                .engine_id
                .or_else(|| Some(vellum_proxy_runtime::codex_local_v0_150::ENGINE_ID.to_string())),
            engine_provenance: meta.engine_provenance.or_else(|| {
                Some(vellum_proxy_runtime::codex_local_v0_150::ENGINE_PROVENANCE.to_string())
            }),
            route_id: meta.route_id,
            upstream_model: meta.upstream_model,
            tokens_before: meta.tokens_before,
            tokens_after: meta.tokens_after,
            elapsed_ms: meta.elapsed_ms,
            kind,
            source_items,
            canonical_sha256: canonical_items_sha256(&canonical_items)?,
            canonical_items,
            portable_items,
            created_at: now_unix_secs(),
            trigger: trigger.to_string(),
            producer_route: producer_route.map(str::to_string),
            producer_provider: producer_provider.map(str::to_string),
            producer_model: producer_model.map(str::to_string),
            canonical_available: true,
            realm_fingerprint: meta.realm_fingerprint,
            parent_compaction_id: meta.parent_compaction_id,
            generation,
            status: CheckpointStatus::Provisional,
            continuation_mode: meta.continuation_mode,
            continuity_kind: meta.continuity_kind,
            strategy: meta.strategy,
            reasoning_context_requested: meta.reasoning_context_requested,
            reasoning_context_effective: meta.reasoning_context_effective,
            contains_encrypted_reasoning,
            reasoning_continuity_verified: false,
            source_head_response_id: meta.source_head_response_id.clone(),
            verified_followup_response_id: None,
            input_tokens: None,
            output_tokens: None,
            dropped_message_count: None,
            checkpoint_schema_version: meta.checkpoint_schema_version,
            source_hash: meta.source_hash,
            checkpoint_hash: meta.checkpoint_hash,
            audit: meta.audit,
        };
        // Journal row + response binding must land together — a checkpoint
        // written without its binding (or vice versa) is unrecoverable by
        // the ordinary replay/candidate-id lookup paths.
        self.write_compaction_journal_with_binding(
            &journal,
            meta.source_head_response_id.as_deref(),
        )?;
        Ok(())
    }

    /// Compaction item ids referenced by a request input (for lifecycle verify).
    pub fn compaction_ids_in_items(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    /// After a successful follow-up response, mark every provisional checkpoint
    /// referenced by the request as verified.
    pub fn verify_request_compactions(
        &self,
        request: &Value,
        followup_response_id: &str,
    ) -> AppResult<usize> {
        let mut ids = Self::compaction_ids_in_items(&request_input_items(request));
        if let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            ids.extend(self.compaction_ids_for_response(previous)?);
        }
        self.verify_compaction_ids(&ids, followup_response_id)
    }

    /// Verify an explicit set of consumed compaction IDs (from continuation
    /// execution context) and supersede their parents.
    pub fn verify_compaction_ids(
        &self,
        compaction_ids: &[String],
        followup_response_id: &str,
    ) -> AppResult<usize> {
        let mut verified = 0usize;
        let mut seen = HashSet::new();
        for compaction_id in compaction_ids {
            if !seen.insert(compaction_id.clone()) {
                continue;
            }
            if self.mark_compaction_verified(compaction_id, followup_response_id)? {
                verified = verified.saturating_add(1);
                if let Some(journal) = self.get_compaction(compaction_id)? {
                    if let Some(parent) = journal.parent_compaction_id.as_deref() {
                        let _ = self.mark_compaction_superseded(parent);
                    }
                }
            }
        }
        Ok(verified)
    }

    /// Latest conversation continuation hints when a request has no
    /// previous_response_id (full Desktop snapshot).
    #[allow(clippy::type_complexity)]
    pub fn latest_conversation_context(
        &self,
        request: &Value,
    ) -> AppResult<Option<(String, Option<String>, Option<String>, Option<String>)>> {
        // (response_id, route_id, realm_fingerprint, model)
        let Some(key) = conversation_key(request) else {
            return Ok(None);
        };
        self.conn
            .lock()
            .query_row(
                "SELECT history.response_id,
                        realm.route_id,
                        realm.realm_fingerprint,
                        realm.model
                   FROM response_history history
                   LEFT JOIN response_realm realm
                     ON realm.response_id = history.response_id
                  WHERE history.conversation_key = ?1
                  ORDER BY history.created_at DESC, history.rowid DESC
                  LIMIT 1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                AppError::Message(format!(
                    "cannot resolve latest conversation continuation context: {error}"
                ))
            })
    }

    /// Mark a provisional checkpoint as verified after the next same-realm
    /// response succeeds. Source windows must remain until this point.
    pub fn mark_compaction_verified(
        &self,
        compaction_id: &str,
        followup_response_id: &str,
    ) -> AppResult<bool> {
        let Some(mut journal) = self.get_compaction(compaction_id)? else {
            return Ok(false);
        };
        journal.status = CheckpointStatus::Verified;
        journal.reasoning_continuity_verified = true;
        journal.verified_followup_response_id = Some(followup_response_id.to_string());
        journal.schema_version = COMPACTION_JOURNAL_SCHEMA_V3;
        self.write_compaction_journal(&journal)?;
        Ok(true)
    }

    /// Supersede an older checkpoint after a recompact generation is verified.
    pub fn mark_compaction_superseded(&self, compaction_id: &str) -> AppResult<bool> {
        let Some(mut journal) = self.get_compaction(compaction_id)? else {
            return Ok(false);
        };
        journal.status = CheckpointStatus::Superseded;
        journal.schema_version = COMPACTION_JOURNAL_SCHEMA_V3;
        self.write_compaction_journal(&journal)?;
        Ok(true)
    }

    /// Mark a checkpoint as Failed after compaction error. Source window must
    /// remain intact for rollback (issue #4 lifecycle).
    pub fn mark_compaction_failed(&self, compaction_id: &str) -> AppResult<bool> {
        let Some(mut journal) = self.get_compaction(compaction_id)? else {
            return Ok(false);
        };
        // Never demote a verified checkpoint through a later failure path.
        if journal.status == CheckpointStatus::Verified {
            return Ok(false);
        }
        journal.status = CheckpointStatus::Failed;
        journal.schema_version = COMPACTION_JOURNAL_SCHEMA_V3;
        self.write_compaction_journal(&journal)?;
        Ok(true)
    }

    /// Parent generation for a newly produced checkpoint must come from the
    /// actually consumed parent, not a guessed counter (issue #4).
    pub fn next_generation_from_parent(
        &self,
        parent_compaction_id: Option<&str>,
    ) -> AppResult<u32> {
        let Some(parent_id) = parent_compaction_id.filter(|id| !id.is_empty()) else {
            return Ok(1);
        };
        let Some(parent) = self.get_compaction(parent_id)? else {
            return Ok(1);
        };
        Ok(parent.generation.saturating_add(1).max(1))
    }

    pub fn save_legacy_recovered_compaction(
        &self,
        compaction_id: &str,
        source_items: Vec<Value>,
        recovered_items: Vec<Value>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> AppResult<()> {
        self.save_canonical_compaction(
            compaction_id,
            CompactionJournalKind::LegacyRecovered,
            source_items,
            recovered_items,
            None,
            "official_recovered",
            None,
            provider,
            model,
        )
    }

    pub fn update_compaction_portable(
        &self,
        compaction_id: &str,
        portable_items: Vec<Value>,
    ) -> AppResult<()> {
        if portable_items.is_empty() {
            return Err(AppError::Message(format!(
                "portable compaction window {compaction_id} cannot be empty"
            )));
        }
        let mut journal = self.get_compaction(compaction_id)?.ok_or_else(|| {
            AppError::Message(format!(
                "compaction journal {compaction_id} disappeared before portable handoff was saved"
            ))
        })?;
        journal.portable_items = Some(portable_items);
        self.write_compaction_journal(&journal)
    }

    fn write_compaction_journal(&self, journal: &CompactionJournal) -> AppResult<()> {
        self.write_compaction_journal_with_binding(journal, None)
    }

    /// Write a compaction journal row and, if given, its `response_compaction`
    /// binding inside a single transaction, so a checkpoint is never
    /// persisted without the mapping that later replay lookup relies on (or
    /// the mapping without the checkpoint it points at).
    fn write_compaction_journal_with_binding(
        &self,
        journal: &CompactionJournal,
        binding_response_id: Option<&str>,
    ) -> AppResult<()> {
        let plaintext = serde_json::to_vec(journal)
            .map_err(|error| AppError::Message(format!("encode compaction journal: {error}")))?;
        let encoded = encode_payload(&plaintext)?;
        let aad = format!("compact:{}", journal.compaction_id);
        let blob = self.cipher.seal(&encoded, aad.as_bytes())?;
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Message(format!("開啟壓縮交易失敗：{error}")))?;
        tx.execute(
            "INSERT INTO compaction_journal (head_response_id, blob, created_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(head_response_id) DO UPDATE SET
               blob = excluded.blob,
               created_at = excluded.created_at",
            params![journal.compaction_id, blob, journal.created_at],
        )
        .map_err(|error| AppError::Message(format!("save compaction journal: {error}")))?;
        if let Some(response_id) = binding_response_id {
            tx.execute(
                "INSERT INTO response_compaction (response_id, compaction_id, relation)
                 VALUES (?1, ?2, 'produced')
                 ON CONFLICT(response_id, compaction_id) DO UPDATE SET relation = 'produced'",
                params![response_id, journal.compaction_id],
            )
            .map_err(|error| {
                AppError::Message(format!("寫入 response_compaction 對應失敗：{error}"))
            })?;
        }
        tx.commit()
            .map_err(|error| AppError::Message(format!("提交壓縮交易失敗：{error}")))?;
        Ok(())
    }

    pub fn latest_compaction(&self) -> AppResult<Option<CompactionJournal>> {
        let row: Option<(String, Vec<u8>)> = self
            .conn
            .lock()
            .query_row(
                "SELECT head_response_id, blob
                 FROM compaction_journal
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| AppError::Message(format!("讀取最新壓縮紀錄失敗：{error}")))?;
        let Some((head_response_id, blob)) = row else {
            return Ok(None);
        };
        let aad = format!("compact:{head_response_id}");
        let plaintext = decode_payload(self.cipher.open(&blob, aad.as_bytes())?)?;
        let journal: CompactionJournal = serde_json::from_slice(&plaintext)
            .map_err(|error| AppError::Message(format!("最新壓縮紀錄解碼失敗：{error}")))?;
        let migrate = journal.schema_version < COMPACTION_JOURNAL_SCHEMA_V3;
        let journal = normalize_compaction_journal(journal)?;
        if migrate {
            self.write_compaction_journal(&journal)?;
        }
        Ok(Some(journal))
    }

    /// Newest compaction bound to one hashed conversation key.
    ///
    /// `compaction_journal` is keyed by checkpoint id, while the conversation
    /// identity lives on `response_history`. `response_compaction` is the
    /// durable edge between them, so the Context screen must follow that edge
    /// instead of returning the newest checkpoint from an unrelated task.
    pub fn latest_compaction_for_conversation(
        &self,
        conversation_key: &str,
    ) -> AppResult<Option<CompactionJournal>> {
        Ok(self
            .recent_compactions_for_conversation(conversation_key, 1)?
            .pop())
    }

    /// The newest `limit` compactions bound to one conversation, newest first.
    ///
    /// The Context screen shows a session's last few handovers rather than one
    /// event, because a single compaction says nothing about whether the
    /// conversation keeps losing the same thing every time.
    ///
    /// A row that fails to decode is skipped rather than failing the whole
    /// read: one damaged journal must not blank out the handovers around it.
    pub fn recent_compactions_for_conversation(
        &self,
        conversation_key: &str,
        limit: u32,
    ) -> AppResult<Vec<CompactionJournal>> {
        let limit = limit.clamp(1, 50);
        let rows: Vec<(String, Vec<u8>)> = {
            let conn = self.conn.lock();
            let mut statement = conn
                .prepare(
                    "SELECT journal.head_response_id, journal.blob
                       FROM compaction_journal AS journal
                      WHERE EXISTS (
                        SELECT 1
                          FROM response_history AS history
                         WHERE history.conversation_key = ?1
                           AND (
                             history.response_id = journal.head_response_id
                             OR EXISTS (
                               SELECT 1
                                 FROM response_compaction AS binding
                                WHERE binding.compaction_id = journal.head_response_id
                                  AND binding.response_id = history.response_id
                             )
                           )
                      )
                      ORDER BY journal.created_at DESC, journal.rowid DESC
                      LIMIT ?2",
                )
                .map_err(|error| AppError::Message(format!("讀取工作階段壓縮紀錄失敗：{error}")))?;
            let mapped = statement
                .query_map(params![conversation_key, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .map_err(|error| AppError::Message(format!("讀取工作階段壓縮紀錄失敗：{error}")))?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Message(format!("讀取工作階段壓縮紀錄失敗：{error}")))?
        };

        let mut journals = Vec::with_capacity(rows.len());
        for (head_response_id, blob) in rows {
            let aad = format!("compact:{head_response_id}");
            let Ok(opened) = self.cipher.open(&blob, aad.as_bytes()) else {
                continue;
            };
            let Ok(plaintext) = decode_payload(opened) else {
                continue;
            };
            let Ok(journal) = serde_json::from_slice::<CompactionJournal>(&plaintext) else {
                continue;
            };
            let migrate = journal.schema_version < COMPACTION_JOURNAL_SCHEMA_V3;
            let journal = normalize_compaction_journal(journal)?;
            if migrate {
                self.write_compaction_journal(&journal)?;
            }
            journals.push(journal);
        }
        Ok(journals)
    }

    pub fn get_compaction(&self, head_response_id: &str) -> AppResult<Option<CompactionJournal>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .lock()
            .query_row(
                "SELECT blob FROM compaction_journal WHERE head_response_id = ?1",
                [head_response_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| AppError::Message(format!("讀取壓縮紀錄失敗：{error}")))?;
        let Some(blob) = blob else {
            return Ok(None);
        };
        let aad = format!("compact:{head_response_id}");
        let plaintext = decode_payload(self.cipher.open(&blob, aad.as_bytes())?)?;
        let journal: CompactionJournal = serde_json::from_slice(&plaintext)
            .map_err(|error| AppError::Message(format!("壓縮紀錄解碼失敗：{error}")))?;
        let migrate = journal.schema_version < COMPACTION_JOURNAL_SCHEMA_V3;
        let journal = normalize_compaction_journal(journal)?;
        if migrate {
            self.write_compaction_journal(&journal)?;
        }
        Ok(Some(journal))
    }

    /// Recover the newest full conversation snapshot that predates an opaque
    /// provider compaction. If retention has already removed every pre-compact
    /// snapshot, preserve the visible portion of the newest snapshot and add a
    /// handoff notice instead of either forwarding an undecryptable item or
    /// silently pretending that the missing provider-owned state is available.
    pub fn recover_pre_compaction_items(&self, request: &Value) -> AppResult<Option<Vec<Value>>> {
        let Some(key) = conversation_key(request) else {
            return Ok(None);
        };
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT response_id, previous_response_id, blob, created_at
                   FROM response_history
                  WHERE conversation_key = ?1
                  ORDER BY created_at DESC, rowid DESC
                  LIMIT ?2",
            )
            .map_err(|error| {
                AppError::Message(format!(
                    "prepare pre-compaction recovery query failed: {error}"
                ))
            })?;
        let rows = statement
            .query_map(
                params![key, KEEP_RECENT_SNAPSHOTS_PER_CONVERSATION as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|error| {
                AppError::Message(format!("query pre-compaction snapshots failed: {error}"))
            })?;

        let mut visible_fallback = None;
        for row in rows {
            let (response_id, previous_response_id, blob, created_at) = row.map_err(|error| {
                AppError::Message(format!("read pre-compaction snapshot failed: {error}"))
            })?;
            let entry = self.open_blob(&response_id, &blob, previous_response_id, created_at)?;
            let mut items = entry.input_items;
            items.extend(entry.output_items);
            if items.iter().any(is_compaction_item) {
                if visible_fallback.is_none() {
                    items.retain(|item| {
                        !is_compaction_item(item)
                            && item.get("type").and_then(Value::as_str)
                                != Some("compaction_trigger")
                    });
                    if !items.is_empty() {
                        items.insert(
                            0,
                            serde_json::json!({
                                "type": "message",
                                "role": "developer",
                                "content": [{
                                    "type": "input_text",
                                    "text": "Provider handoff notice: an earlier OpenAI-only compacted state is not decryptable by the selected provider. Continue from the preserved visible conversation and do not invent details that are not present."
                                }]
                            }),
                        );
                        visible_fallback = Some(items);
                    }
                }
                continue;
            }
            items.retain(|item| {
                item.get("type").and_then(Value::as_str) != Some("compaction_trigger")
            });
            if !items.is_empty() {
                return Ok(Some(items));
            }
        }
        Ok(visible_fallback)
    }

    pub fn restore_compaction(&self, head_response_id: &str) -> AppResult<bool> {
        let deleted = self
            .conn
            .lock()
            .execute(
                "DELETE FROM compaction_journal WHERE head_response_id = ?1",
                [head_response_id],
            )
            .map_err(|error| AppError::Message(format!("還原壓縮失敗：{error}")))?;
        Ok(deleted > 0)
    }

    /// Resolve the nearest compaction checkpoint in `head`'s ancestry and
    /// return a replay chain beginning with the checkpoint replacement,
    /// followed by every completed exchange after that checkpoint.
    ///
    /// This makes a checkpoint durable across subsequent response IDs instead
    /// of applying it to only the first post-compaction request.
    pub fn compacted_replay_chain(&self, head: &str) -> AppResult<Option<Vec<HistoryEntry>>> {
        let mut current = Some(head.to_string());
        let mut seen = HashSet::new();
        let mut descendants_newest_first = Vec::new();
        // Older Vellum builds bound a consumed checkpoint to the response but
        // persisted the pre-materialization client request. Such rows contain
        // neither the cmp_* item nor a canonical/source prefix. Walk through
        // those descendants and attach the canonical window once the chain
        // ends instead of rejecting an otherwise recoverable conversation.
        let mut deferred_consumed_journal: Option<CompactionJournal> = None;

        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break;
            }

            // 1) Legacy / test path: journal keyed by the same id.
            // 2) Production path: journal keyed by cmp_* and linked via mapping
            //    or the response's compaction output items.
            let mut candidate_ids = vec![id.clone()];
            let bindings = self.compaction_bindings_for_response(&id)?;
            candidate_ids.extend(bindings.iter().map(|(cid, _)| cid.clone()));

            let row: Option<(Option<String>, Vec<u8>, i64)> = {
                let conn = self.conn.lock();
                conn.query_row(
                    "SELECT previous_response_id, blob, created_at
                     FROM response_history WHERE response_id = ?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|error| {
                    AppError::Message(format!("讀取 checkpoint 後續歷史失敗：{error}"))
                })?
            };

            if let Some((previous, blob, created_at)) = row {
                let entry = self.open_blob(&id, &blob, previous.clone(), created_at)?;
                for cid in Self::compaction_ids_in_items(&entry.input_items) {
                    if !candidate_ids.iter().any(|existing| existing == &cid) {
                        candidate_ids.push(cid);
                    }
                }
                for item in &entry.output_items {
                    if item.get("type").and_then(Value::as_str) == Some("compaction") {
                        if let Some(cid) = item.get("id").and_then(Value::as_str) {
                            if !candidate_ids.iter().any(|existing| existing == cid) {
                                candidate_ids.push(cid.to_string());
                            }
                        }
                    }
                }

                for candidate in &candidate_ids {
                    if let Some(journal) = self.get_compaction(candidate)? {
                        if !journal.canonical_available {
                            return Err(AppError::Message(format!(
                                "compaction checkpoint {} has no canonical window",
                                journal.compaction_id
                            )));
                        }
                        // A checkpoint in this request input is a boundary
                        // *before* the current response. Preserve only the
                        // request suffix after that reference plus this
                        // response's output. A produced checkpoint (found in
                        // response output / legacy mapping) is already the
                        // boundary after this response and must not duplicate
                        // the producer exchange.
                        let consumed_at = entry.input_items.iter().rposition(|item| {
                            item.get("type").and_then(Value::as_str) == Some("compaction")
                                && item.get("id").and_then(Value::as_str)
                                    == Some(candidate.as_str())
                        });
                        let mapped_as_consumed = bindings
                            .iter()
                            .any(|(cid, relation)| cid == candidate && relation == "consumed");
                        if let Some(index) = consumed_at {
                            descendants_newest_first.push(HistoryEntry {
                                response_id: entry.response_id.clone(),
                                previous_response_id: None,
                                input_items: entry.input_items[index + 1..].to_vec(),
                                output_items: entry.output_items.clone(),
                                created_at: entry.created_at,
                            });
                        } else if mapped_as_consumed {
                            let input_items = if entry
                                .input_items
                                .starts_with(journal.source_items.as_slice())
                            {
                                entry.input_items[journal.source_items.len()..].to_vec()
                            } else if entry
                                .input_items
                                .starts_with(journal.canonical_items.as_slice())
                            {
                                entry.input_items[journal.canonical_items.len()..].to_vec()
                            } else {
                                deferred_consumed_journal.get_or_insert_with(|| journal.clone());
                                log::warn!(
                                    "[Compaction] response {} predates materialized boundary persistence for {}; recovering through its response ancestry",
                                    entry.response_id,
                                    journal.compaction_id
                                );
                                continue;
                            };
                            descendants_newest_first.push(HistoryEntry {
                                response_id: entry.response_id.clone(),
                                previous_response_id: None,
                                input_items,
                                output_items: entry.output_items.clone(),
                                created_at: entry.created_at,
                            });
                        }
                        descendants_newest_first.reverse();
                        let mut replay = vec![HistoryEntry {
                            response_id: format!("checkpoint:{}", journal.compaction_id),
                            previous_response_id: None,
                            input_items: journal.canonical_items,
                            output_items: Vec::new(),
                            created_at: journal.created_at,
                        }];
                        replay.extend(descendants_newest_first);
                        return Ok(Some(replay));
                    }
                }

                descendants_newest_first.push(entry);
                current = previous;
            } else {
                // No history row; still try journal ids alone (compact-only head).
                for candidate in &candidate_ids {
                    if let Some(journal) = self.get_compaction(candidate)? {
                        if !journal.canonical_available {
                            return Err(AppError::Message(format!(
                                "compaction checkpoint {} has no canonical window",
                                journal.compaction_id
                            )));
                        }
                        descendants_newest_first.reverse();
                        let mut replay = vec![HistoryEntry {
                            response_id: format!("checkpoint:{}", journal.compaction_id),
                            previous_response_id: None,
                            input_items: journal.canonical_items,
                            output_items: Vec::new(),
                            created_at: journal.created_at,
                        }];
                        replay.extend(descendants_newest_first);
                        return Ok(Some(replay));
                    }
                }
                break;
            }
        }
        if let Some(journal) = deferred_consumed_journal {
            descendants_newest_first.reverse();
            let mut replay = vec![HistoryEntry {
                response_id: format!("checkpoint:{}", journal.compaction_id),
                previous_response_id: None,
                input_items: journal.canonical_items,
                output_items: Vec::new(),
                created_at: journal.created_at,
            }];
            replay.extend(descendants_newest_first);
            return Ok(Some(replay));
        }
        Ok(None)
    }

    /// Reconstruct the auditable pre-compaction source for a descendant
    /// response. Unlike `compacted_replay_chain`, this deliberately uses the
    /// journal's source window and is only used to create a later canonical or
    /// portable checkpoint; it is never forwarded for same-realm official
    /// continuation.
    pub fn compaction_source_for_chain(&self, head: &str) -> AppResult<Option<Vec<Value>>> {
        let conn = self.conn.lock();
        let mut current = Some(head.to_string());
        let mut seen = HashSet::new();
        let mut descendants_newest_first = Vec::<HistoryEntry>::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break;
            }
            let journal_blob: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT blob FROM compaction_journal WHERE head_response_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    AppError::Message(format!("query compaction source checkpoint: {error}"))
                })?;
            if let Some(blob) = journal_blob {
                let aad = format!("compact:{id}");
                let plaintext = decode_payload(self.cipher.open(&blob, aad.as_bytes())?)?;
                let journal = normalize_compaction_journal(
                    serde_json::from_slice(&plaintext).map_err(|error| {
                        AppError::Message(format!("decode compaction source checkpoint: {error}"))
                    })?,
                )?;
                let mut source = journal.source_items;
                descendants_newest_first.reverse();
                for entry in descendants_newest_first {
                    source.extend(entry.input_items);
                    source.extend(entry.output_items);
                }
                return Ok(Some(source));
            }
            let row: Option<(Option<String>, Vec<u8>, i64)> = conn
                .query_row(
                    "SELECT previous_response_id, blob, created_at
                     FROM response_history WHERE response_id = ?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|error| {
                    AppError::Message(format!("query compaction source ancestry: {error}"))
                })?;
            let Some((previous, blob, created_at)) = row else {
                break;
            };
            descendants_newest_first.push(self.open_blob(
                &id,
                &blob,
                previous.clone(),
                created_at,
            )?);
            current = previous;
        }
        Ok(None)
    }

    pub fn restore_compaction_for_chain(&self, head: &str) -> AppResult<bool> {
        let conn = self.conn.lock();
        let mut current = Some(head.to_string());
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break;
            }
            let deleted = conn
                .execute(
                    "DELETE FROM compaction_journal WHERE head_response_id = ?1",
                    [&id],
                )
                .map_err(|error| AppError::Message(format!("還原壓縮失敗：{error}")))?;
            if deleted > 0 {
                return Ok(true);
            }
            current = conn
                .query_row(
                    "SELECT previous_response_id FROM response_history WHERE response_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    AppError::Message(format!("查找壓縮 checkpoint 祖先失敗：{error}"))
                })?
                .flatten();
        }
        Ok(false)
    }

    fn open_blob(
        &self,
        response_id: &str,
        blob: &[u8],
        previous_response_id: Option<String>,
        created_at: i64,
    ) -> AppResult<HistoryEntry> {
        let encoded = self.cipher.open(blob, response_id.as_bytes())?;
        let plaintext = decode_payload(encoded)?;
        let body: PayloadBody = serde_json::from_slice(&plaintext)
            .map_err(|e| AppError::Message(format!("歷史內容解碼失敗：{e}")))?;
        Ok(HistoryEntry {
            response_id: response_id.to_string(),
            previous_response_id,
            input_items: body.input_items,
            output_items: body.output_items,
            created_at,
        })
    }
}

fn encode_payload(plaintext: &[u8]) -> AppResult<Vec<u8>> {
    zstd::stream::encode_all(plaintext, 3)
        .map_err(|error| AppError::Message(format!("壓縮歷史內容失敗：{error}")))
}

fn decode_payload(encoded: Vec<u8>) -> AppResult<Vec<u8>> {
    if !encoded.starts_with(ZSTD_MAGIC) {
        return Ok(encoded);
    }
    zstd::stream::decode_all(encoded.as_slice())
        .map_err(|error| AppError::Message(format!("解壓歷史內容失敗：{error}")))
}

/// Contain a panic inside a history operation so it surfaces as a specific,
/// reported [`AppError::HistoryUnavailable`] at the Tauri command boundary
/// instead of unwinding through command dispatch with no clear signal. Call
/// this from command handlers that touch [`HistoryStore`], wrapping the
/// whole history-touching body.
///
/// This is deliberately *not* what makes recovery safe by itself — the
/// underlying `conn: parking_lot::Mutex<Connection>` never poisons, so other
/// history calls (and unrelated proxy/session functionality) keep working
/// whether or not the panic is caught here. This wrapper exists purely so the
/// *caller* of the panicking operation gets a clean error instead of an
/// opaque crashed command.
pub fn catch_history_panic<T>(
    op: impl FnOnce() -> AppResult<T> + std::panic::UnwindSafe,
) -> AppResult<T> {
    match std::panic::catch_unwind(op) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|value| value.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log::error!("[History] operation panicked: {message}");
            Err(AppError::HistoryUnavailable(message))
        }
    }
}

/// Rows staged for the one-time env-key → platform-key migration
/// (`crypto::JournalCipher`'s recovery path). Nothing is written to disk
/// until every row above has been decrypted under the old key, re-encrypted
/// under the new key, and had that round trip verified — see
/// [`prepare_journal_migration`] / [`commit_journal_migration`].
pub struct StagedJournalMigration {
    db_path: PathBuf,
    history_rows: Vec<(String, Vec<u8>)>,
    journal_rows: Vec<(String, Vec<u8>)>,
}

/// Decrypt every `response_history` and `compaction_journal` blob under
/// `old`, re-encrypt under `new`, and verify each round trip — entirely in
/// memory, no disk writes. Returns an empty staged migration when the DB
/// doesn't exist yet (nothing to migrate). On any decrypt/verify failure,
/// returns an error and leaves the on-disk file completely untouched.
pub fn prepare_journal_migration(
    db_path: &Path,
    old: &JournalCipher,
    new: &JournalCipher,
) -> AppResult<StagedJournalMigration> {
    if !db_path.exists() {
        return Ok(StagedJournalMigration {
            db_path: db_path.to_path_buf(),
            history_rows: Vec::new(),
            journal_rows: Vec::new(),
        });
    }
    let conn = Connection::open(db_path)
        .map_err(|error| AppError::Message(format!("開啟歷史庫以進行金鑰遷移失敗：{error}")))?;

    let history_source: Vec<(String, Vec<u8>)> = {
        let mut statement = conn
            .prepare("SELECT response_id, blob FROM response_history")
            .map_err(|error| AppError::Message(format!("準備歷史遷移查詢失敗：{error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| AppError::Message(format!("讀取待遷移歷史失敗：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("解析待遷移歷史失敗：{error}")))?
    };
    let journal_source: Vec<(String, Vec<u8>)> = {
        let mut statement = conn
            .prepare("SELECT head_response_id, blob FROM compaction_journal")
            .map_err(|error| AppError::Message(format!("準備壓縮紀錄遷移查詢失敗：{error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| AppError::Message(format!("讀取待遷移壓縮紀錄失敗：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("解析待遷移壓縮紀錄失敗：{error}")))?
    };
    drop(conn);

    let mut history_rows = Vec::with_capacity(history_source.len());
    for (response_id, blob) in history_source {
        let plaintext = old.open(&blob, response_id.as_bytes())?;
        let sealed = new.seal(&plaintext, response_id.as_bytes())?;
        if new.open(&sealed, response_id.as_bytes())? != plaintext {
            return Err(AppError::Message(format!(
                "history re-encryption verification failed for {response_id}; original data was not modified"
            )));
        }
        history_rows.push((response_id, sealed));
    }
    let mut journal_rows = Vec::with_capacity(journal_source.len());
    for (head_id, blob) in journal_source {
        let aad = format!("compact:{head_id}");
        let plaintext = old.open(&blob, aad.as_bytes())?;
        let sealed = new.seal(&plaintext, aad.as_bytes())?;
        if new.open(&sealed, aad.as_bytes())? != plaintext {
            return Err(AppError::Message(format!(
                "compaction journal re-encryption verification failed for {head_id}; original data was not modified"
            )));
        }
        journal_rows.push((head_id, sealed));
    }

    Ok(StagedJournalMigration {
        db_path: db_path.to_path_buf(),
        history_rows,
        journal_rows,
    })
}

/// Commit a [`StagedJournalMigration`] prepared by [`prepare_journal_migration`]
/// in a single transaction. Every row was already decrypted, re-encrypted,
/// and verified before this is called, so the only remaining failure mode is
/// an I/O error while committing — in that case the transaction rolls back
/// and the file is left exactly as it was.
pub fn commit_journal_migration(staged: StagedJournalMigration) -> AppResult<()> {
    if staged.history_rows.is_empty() && staged.journal_rows.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(&staged.db_path)
        .map_err(|error| AppError::Message(format!("開啟歷史庫以提交金鑰遷移失敗：{error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| AppError::Message(format!("開啟金鑰遷移交易失敗：{error}")))?;
    for (response_id, blob) in &staged.history_rows {
        tx.execute(
            "UPDATE response_history SET blob = ?1 WHERE response_id = ?2",
            params![blob, response_id],
        )
        .map_err(|error| AppError::Message(format!("寫入遷移後歷史列失敗：{error}")))?;
    }
    for (head_id, blob) in &staged.journal_rows {
        tx.execute(
            "UPDATE compaction_journal SET blob = ?1 WHERE head_response_id = ?2",
            params![blob, head_id],
        )
        .map_err(|error| AppError::Message(format!("寫入遷移後壓縮紀錄失敗：{error}")))?;
    }
    tx.commit()
        .map_err(|error| AppError::Message(format!("提交金鑰遷移交易失敗：{error}")))?;
    Ok(())
}

/// Conversation identity shared by history, SessionStatus.id, and session
/// compaction policy lookup (SHA256 of prompt_cache_key / thread / conversation).
pub fn conversation_key(request: &Value) -> Option<String> {
    conversation_key_from_raw(
        request
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .pointer("/metadata/thread_id")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                request
                    .pointer("/metadata/conversation_id")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                request
                    .pointer("/metadata/session_id")
                    .and_then(Value::as_str)
            }),
    )
}

/// Hash a raw conversation/session identifier the same way as [`conversation_key`].
pub fn conversation_key_from_raw(raw: Option<&str>) -> Option<String> {
    raw.filter(|value| !value.trim().is_empty()).map(|value| {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(value.trim().as_bytes()))
    })
}

impl HistoryStore {
    /// Durable recovery of conversation identity for a response id (restart /
    /// registry miss / LRU eviction — issue #4 comment 5146162101 B-2).
    pub fn response_conversation_key(&self, response_id: &str) -> AppResult<Option<String>> {
        if response_id.trim().is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT conversation_key FROM response_history WHERE response_id = ?1",
            params![response_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| AppError::Message(format!("read response conversation_key: {error}")))
        .map(|row| row.flatten().filter(|key| !key.trim().is_empty()))
    }

    /// Number of completed exchanges for one route in a conversation.
    ///
    /// Grok's `x-grok-turn-idx` is route-session scoped. Counting only Grok
    /// exchanges prevents OpenAI or other provider turns from advancing it.
    pub fn conversation_route_exchange_count(
        &self,
        conversation_key: &str,
        route_id: &str,
    ) -> AppResult<u64> {
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*)
                   FROM response_history AS history
                   JOIN response_realm AS realm
                     ON realm.response_id = history.response_id
                  WHERE history.conversation_key = ?1
                    AND realm.route_id = ?2",
                params![conversation_key, route_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                AppError::Message(format!(
                    "query conversation route exchange count failed: {error}"
                ))
            })?;
        Ok(count.max(0) as u64)
    }

    /// Test-only: destroy response_history so durable lookups return a real store error.
    #[cfg(test)]
    pub fn break_response_history_schema_for_test(&self) -> AppResult<()> {
        let conn = self.conn.lock();
        conn.execute_batch("DROP TABLE IF EXISTS response_history")
            .map_err(|error| AppError::Message(format!("break response_history for test: {error}")))
    }
}

fn is_compaction_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("compaction")
}

fn response_context_tokens(response: &Value) -> Option<i64> {
    let usage = response.get("usage")?;
    usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            let input = usage.get("input_tokens").and_then(Value::as_i64)?;
            let output = usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            Some(input.saturating_add(output))
        })
        .filter(|tokens| *tokens > 0)
}

fn prune_conversation_snapshots(conn: &Connection, key: &str, keep: usize) -> AppResult<usize> {
    conn.execute(
        "DELETE FROM response_history
         WHERE conversation_key = ?1
           AND response_id NOT IN (
             SELECT response_id
             FROM response_history
             WHERE conversation_key = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?2
           )
           AND response_id NOT IN (
             SELECT previous_response_id
             FROM response_history
             WHERE previous_response_id IS NOT NULL
           )
           AND response_id NOT IN (
             SELECT head_response_id FROM compaction_journal
           )",
        params![key, keep as i64],
    )
    .map_err(|error| AppError::Message(format!("清理重複對話快照失敗：{error}")))
}

fn pragma_u64(conn: &Connection, name: &str) -> AppResult<u64> {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .map_err(|error| AppError::Message(format!("讀取 SQLite {name} 失敗：{error}")))
}

fn read_telemetry_counter(conn: &Connection, key: &str) -> AppResult<u64> {
    conn.query_row(
        "SELECT value FROM history_telemetry WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()
    .map(|value| value.unwrap_or(0))
    .map_err(|error| AppError::Message(format!("讀取 history telemetry 失敗：{error}")))
}

fn read_telemetry_counter_opt(conn: &Connection, key: &str) -> AppResult<Option<i64>> {
    let value = read_telemetry_counter(conn, key)?;
    Ok(if value == 0 { None } else { Some(value as i64) })
}

fn set_telemetry_counter(conn: &Connection, key: &str, value: u64) -> AppResult<()> {
    conn.execute(
        "INSERT INTO history_telemetry (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value as i64],
    )
    .map_err(|error| AppError::Message(format!("寫入 history telemetry 失敗：{error}")))?;
    Ok(())
}

fn bump_telemetry_counter(conn: &Connection, key: &str, by: u64) -> AppResult<()> {
    let next = read_telemetry_counter(conn, key)?.saturating_add(by);
    set_telemetry_counter(conn, key, next)
}

/// 把 request.body 的 input 正規化成 items 陣列（純函式）。
/// - 陣列：原樣回傳
/// - 物件：包成單元素陣列
/// - 字串：包成標準 user message item
pub fn request_input_items(request: &Value) -> Vec<Value> {
    request
        .get("input")
        .cloned()
        .map(input_value_items)
        .unwrap_or_default()
}

/// 把 input 值正規化成 items（純函式，doc/05 與 cc-switch 對齊）。
pub fn input_value_items(input: Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items,
        Value::Object(object) => vec![Value::Object(object)],
        Value::String(text) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })],
        _ => Vec::new(),
    }
}

/// 把一條鏈攤成 input items：每輪的 input_items 接著 output_items，
/// 最後再接上「目前這一輪」的新 items。順序 oldest → newest（純函式）。
pub fn flatten_chain_items(chain: &[HistoryEntry], current_items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for entry in chain {
        out.extend(entry.input_items.iter().cloned());
        out.extend(entry.output_items.iter().cloned());
    }
    out.extend(current_items.iter().cloned());
    out
}

/// 粗估 items 的 token 數（純函式）。
pub fn estimate_items_tokens(items: &[Value]) -> usize {
    let bytes: usize = items
        .iter()
        .map(|item| serde_json::to_vec(item).map(|b| b.len()).unwrap_or(0))
        .sum();
    bytes.saturating_div(BYTES_PER_TOKEN).max(1)
}

/// 把 `previous_response_id` 鏈攤回請求的 input，並移除該欄位。
/// 回傳攤回與 emergency truncation 指標。純函式——鏈由呼叫端先用
/// `get_chain` 取好傳進來，這裡不碰磁碟。
///
/// `token_budget` 用於截斷：攤回後若超過預算，從最舊端砍掉。這是
/// **without compaction** 的 emergency drain，呼叫端必須計入 telemetry。
pub fn hydrate_input(
    body: &mut Value,
    chain: &[HistoryEntry],
    token_budget: usize,
) -> HydrateResult {
    let had_previous = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());

    // 移除 previous_response_id（上游看不懂，doc/05）。
    if had_previous {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("previous_response_id");
        }
    }

    let Some(input) = body.get_mut("input") else {
        return HydrateResult::default();
    };
    let original = std::mem::take(input);
    let current_items = input_value_items(original);

    if chain.is_empty() {
        // 沒歷史可攤：把 input 原樣放回。
        *input = if current_items.len() == 1 {
            current_items.into_iter().next().unwrap_or(Value::Null)
        } else {
            Value::Array(current_items)
        };
        return HydrateResult::default();
    }

    let mut merged = flatten_chain_items(chain, &current_items);
    let history_count = merged.len() - current_items.len();

    // 預算截斷：從最舊端砍，直到估計 token 數在預算內。
    // 但絕不砍掉「目前這一輪」的 items（current_items）。
    let protect = current_items.len();
    let mut emergency_truncated_items = 0usize;
    while estimate_items_tokens(&merged) > token_budget && merged.len() > protect {
        let removable = merged.len().saturating_sub(protect);
        let next_user = merged
            .iter()
            .enumerate()
            .skip(1)
            .take(removable.saturating_sub(1))
            .find_map(|(index, item)| {
                (item.get("role").and_then(Value::as_str) == Some("user")).then_some(index)
            });
        let drain = next_user.unwrap_or(removable).max(1).min(removable);
        merged.drain(0..drain);
        emergency_truncated_items = emergency_truncated_items.saturating_add(drain);
    }

    *input = Value::Array(merged);
    HydrateResult {
        restored_items: history_count.saturating_sub(emergency_truncated_items),
        emergency_truncated_items,
    }
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_cipher() -> JournalCipher {
        // 固定 32 byte key，測試可重現。
        JournalCipher::from_key(vec![0x42u8; 32], "test").unwrap()
    }

    fn tmp_store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("hist.db");
        let store = HistoryStore::open_with_cipher(db, test_cipher()).unwrap();
        (dir, store)
    }

    #[test]
    fn a_recovery_snapshot_survives_reopening_the_database() {
        // The whole point of the durable half: recovery state must outlive the
        // process, so a restart does not forget that this conversation already
        // recovered.
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("hist.db");

        {
            let store = HistoryStore::open_with_cipher(db.clone(), test_cipher()).unwrap();
            assert!(store.recovery_snapshot_blob("conv-1").unwrap().is_none());
            store
                .put_recovery_snapshot_blob("conv-1", br#"{"conversationKey":"conv-1"}"#, 42)
                .unwrap();
        }

        let reopened = HistoryStore::open_with_cipher(db, test_cipher()).unwrap();
        let blob = reopened
            .recovery_snapshot_blob("conv-1")
            .unwrap()
            .expect("snapshot must survive the reopen");
        assert_eq!(blob, br#"{"conversationKey":"conv-1"}"#);
        // Keyed by conversation, so another conversation never inherits it.
        assert!(reopened.recovery_snapshot_blob("conv-2").unwrap().is_none());
    }

    #[test]
    fn putting_a_recovery_snapshot_twice_replaces_it() {
        let (_dir, store) = tmp_store();
        store
            .put_recovery_snapshot_blob("conv-1", b"first", 1)
            .unwrap();
        store
            .put_recovery_snapshot_blob("conv-1", b"second", 2)
            .unwrap();
        assert_eq!(
            store.recovery_snapshot_blob("conv-1").unwrap().unwrap(),
            b"second"
        );
    }

    #[test]
    fn response_compaction_relation_migrates_existing_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE response_compaction (
                    response_id TEXT NOT NULL,
                    compaction_id TEXT NOT NULL,
                    PRIMARY KEY (response_id, compaction_id)
                 );",
            )
            .unwrap();
        }
        let store = HistoryStore::open_with_cipher(db, test_cipher()).unwrap();
        store
            .bind_consumed_response_compaction("resp", "cmp")
            .unwrap();
        assert_eq!(
            store.compaction_bindings_for_response("resp").unwrap(),
            vec![("cmp".to_string(), "consumed".to_string())]
        );
    }

    #[test]
    fn input_value_items_from_array() {
        let items = input_value_items(json!([{"type": "message"}]));
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn input_value_items_from_string() {
        let items = input_value_items(json!("你好"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["text"], "你好");
    }

    #[test]
    fn input_value_items_from_object() {
        let items = input_value_items(json!({"foo": "bar"}));
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn flatten_chain_preserves_order() {
        let chain = vec![
            HistoryEntry {
                response_id: "a".into(),
                previous_response_id: None,
                input_items: vec![json!({"i": "a1"})],
                output_items: vec![json!({"o": "a1"})],
                created_at: 1,
            },
            HistoryEntry {
                response_id: "b".into(),
                previous_response_id: Some("a".into()),
                input_items: vec![json!({"i": "b1"})],
                output_items: vec![json!({"o": "b1"})],
                created_at: 2,
            },
        ];
        let current = vec![json!({"i": "c1"})];
        let flat = flatten_chain_items(&chain, &current);
        // a1, oa1, b1, ob1, c1
        assert_eq!(flat.len(), 5);
        assert_eq!(flat[0]["i"], "a1");
        assert_eq!(flat[4]["i"], "c1");
    }

    #[test]
    fn hydrate_removes_previous_response_id_and_prepends_history() {
        let chain = vec![HistoryEntry {
            response_id: "a".into(),
            previous_response_id: None,
            input_items: vec![json!({"role": "user", "content": "第一輪"})],
            output_items: vec![json!({"role": "assistant", "content": "回覆"})],
            created_at: 1,
        }];
        let mut body = json!({
            "previous_response_id": "a",
            "input": [{"role": "user", "content": "第二輪"}]
        });
        let restored = hydrate_input(&mut body, &chain, 1_000_000);
        assert_eq!(restored.restored_items, 2);
        assert_eq!(restored.emergency_truncated_items, 0);
        assert!(body.get("previous_response_id").is_none());
        let input = body["input"].as_array().unwrap();
        // 第一輪 user, 回覆 assistant, 第二輪 user
        assert_eq!(input.len(), 3);
        assert_eq!(input[2]["content"], "第二輪");
    }

    #[test]
    fn hydrate_no_chain_keeps_input_and_removes_field() {
        let mut body = json!({
            "previous_response_id": "missing",
            "input": [{"role": "user", "content": "hi"}]
        });
        let restored = hydrate_input(&mut body, &[], 1_000_000);
        assert_eq!(restored.restored_items, 0);
        assert_eq!(restored.emergency_truncated_items, 0);
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn hydrate_truncates_to_budget_protecting_current_items() {
        let chain = vec![HistoryEntry {
            response_id: "a".into(),
            previous_response_id: None,
            input_items: vec![
                json!({"role": "user", "content": "一段很長很長的歷史文字內容用來超過預算"}),
            ],
            output_items: vec![
                json!({"role": "assistant", "content": "也很長的回覆內容同樣要超過預算限制"}),
            ],
            created_at: 1,
        }];
        let mut body = json!({
            "previous_response_id": "a",
            "input": [{"role": "user", "content": "新"}]
        });
        // 預算設極小 → 歷史會被砍掉，但「新」一定要留。
        let result = hydrate_input(&mut body, &chain, 1);
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["content"] == "新"));
        assert!(
            result.emergency_truncated_items > 0,
            "budget overflow must count as emergency truncation without compaction"
        );
    }

    #[test]
    fn emergency_truncation_counter_is_durable() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("hist.db");
        {
            let store = HistoryStore::open_with_cipher(db.clone(), test_cipher()).unwrap();
            store.record_emergency_history_truncation(3).unwrap();
            assert_eq!(store.history_truncated_without_compaction(), 1);
            let telemetry = store.storage_telemetry().unwrap();
            assert_eq!(telemetry.history_truncated_without_compaction, 1);
            assert_eq!(telemetry.history_truncated_items, 3);
            assert!(telemetry.main_db_physical_bytes > 0);
        }
        let reopened = HistoryStore::open_with_cipher(db, test_cipher()).unwrap();
        assert_eq!(reopened.history_truncated_without_compaction(), 1);
        assert_eq!(
            reopened
                .storage_telemetry()
                .unwrap()
                .history_truncated_items,
            3
        );
    }

    #[test]
    fn journal_retention_is_reference_aware() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction(
                "cmp_old_verified",
                CompactionJournalKind::Official,
                vec![json!("source-old")],
                vec![json!({"type": "compaction", "id": "cmp_old_verified"})],
                None,
                "official",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .mark_compaction_verified("cmp_old_verified", "resp_old")
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE compaction_journal SET created_at = 1 WHERE head_response_id = ?1",
                ["cmp_old_verified"],
            )
            .unwrap();
        }
        store
            .save_canonical_compaction(
                "cmp_new_verified",
                CompactionJournalKind::Official,
                vec![json!("source-new")],
                vec![json!({"type": "compaction", "id": "cmp_new_verified"})],
                None,
                "official",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .mark_compaction_verified("cmp_new_verified", "resp_new")
            .unwrap();
        // Referenced old verified must survive even past the cutoff.
        store
            .record_exchange(
                &json!({"input": "x"}),
                &json!({"id": "resp_ref", "output": []}),
            )
            .unwrap();
        store
            .bind_response_compaction("resp_ref", "cmp_old_verified")
            .unwrap();
        // Unreferenced provisional past cutoff is aged out.
        store
            .save_canonical_compaction(
                "cmp_provisional",
                CompactionJournalKind::Local,
                vec![json!("source-p")],
                vec![json!({"type": "compaction", "id": "cmp_provisional"})],
                None,
                "local",
                None,
                None,
                None,
            )
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE compaction_journal SET created_at = 1 WHERE head_response_id = ?1",
                ["cmp_provisional"],
            )
            .unwrap();
        }
        let deleted = store.evict_stale_compaction_journals(1).unwrap();
        assert!(deleted >= 1);
        assert!(store.get_compaction("cmp_old_verified").unwrap().is_some());
        assert!(store.get_compaction("cmp_new_verified").unwrap().is_some());
        assert!(store.get_compaction("cmp_provisional").unwrap().is_none());
    }

    #[test]
    fn cleanup_removes_dangling_response_compaction_rows() {
        let (_dir, store) = tmp_store();
        store
            .bind_response_compaction("resp_missing", "cmp_missing")
            .unwrap();
        let deleted = store.cleanup_dangling_response_compaction().unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn stopped_state_maintenance_reports_checkpoint() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": "hi"}),
                &json!({"id": "resp_m", "output": []}),
            )
            .unwrap();
        let report = store
            .run_stopped_state_maintenance(DEFAULT_RETENTION_DAYS)
            .unwrap();
        assert!(report.telemetry.last_maintenance_at.is_some());
        // Checkpoint may report false on some SQLite builds if there is no WAL
        // activity, but maintenance always stamps last_maintenance_at.
        let again = store.storage_telemetry().unwrap();
        assert_eq!(
            again.history_truncated_without_compaction,
            report.telemetry.history_truncated_without_compaction
        );
        assert!(again.logical_live_bytes > 0 || again.main_db_physical_bytes > 0);
    }

    #[test]
    fn record_and_get_roundtrip() {
        let (_dir, store) = tmp_store();
        let req = json!({"previous_response_id": null, "input": "嗨"});
        let resp = json!({"id": "resp_1", "output": [{"type": "message", "content": "嗨嗨"}]});
        assert!(store.record_exchange(&req, &resp).unwrap());

        let entry = store.get("resp_1").unwrap().unwrap();
        assert_eq!(entry.response_id, "resp_1");
        assert_eq!(entry.input_items.len(), 1);
        assert_eq!(entry.input_items[0]["content"][0]["text"], "嗨");
        assert_eq!(entry.output_items[0]["content"], "嗨嗨");
    }

    #[test]
    fn full_context_snapshots_are_compressed_and_bounded_per_conversation() {
        let (_dir, store) = tmp_store();
        let repeated = "context ".repeat(20_000);
        for index in 0..12 {
            store
                .record_exchange(
                    &json!({
                        "prompt_cache_key": "thread-1",
                        "input": [{"role": "user", "content": repeated}]
                    }),
                    &json!({
                        "id": format!("snapshot-{index}"),
                        "output": [{"type": "message", "content": "ok"}]
                    }),
                )
                .unwrap();
        }

        let conn = store.conn.lock();
        let count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM response_history WHERE conversation_key IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (payload, blob): (u64, u64) = conn
            .query_row(
                "SELECT payload_bytes, length(blob)
                 FROM response_history
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, KEEP_RECENT_SNAPSHOTS_PER_CONVERSATION);
        assert!(blob < payload / 4, "compressed={blob}, original={payload}");
    }

    #[test]
    fn legacy_uncompressed_history_remains_readable() {
        let (_dir, store) = tmp_store();
        let plaintext = serde_json::to_vec(&PayloadBody {
            input_items: vec![json!({"role": "user", "content": "legacy"})],
            output_items: vec![json!({"role": "assistant", "content": "ok"})],
        })
        .unwrap();
        let blob = store.cipher.seal(&plaintext, b"legacy-response").unwrap();
        store
            .conn
            .lock()
            .execute(
                "INSERT INTO response_history
                   (response_id, previous_response_id, blob, payload_bytes, created_at)
                 VALUES ('legacy-response', NULL, ?1, ?2, 1)",
                params![blob, plaintext.len() as i64],
            )
            .unwrap();

        let entry = store.get("legacy-response").unwrap().unwrap();
        assert_eq!(entry.input_items[0]["content"], "legacy");
    }

    #[test]
    fn record_without_id_is_noop() {
        let (_dir, store) = tmp_store();
        let req = json!({"input": "x"});
        let resp = json!({"output": []});
        assert!(!store.record_exchange(&req, &resp).unwrap());
    }

    #[test]
    fn chain_walks_previous_response_id() {
        let (_dir, store) = tmp_store();
        // resp_1 ← resp_2 ← resp_3
        store
            .record_exchange(
                &json!({"input": "第一輪"}),
                &json!({"id": "resp_1", "output": [{"c": "a1"}]}),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "resp_1", "input": "第二輪"}),
                &json!({"id": "resp_2", "output": [{"c": "a2"}]}),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "resp_2", "input": "第三輪"}),
                &json!({"id": "resp_3", "output": [{"c": "a3"}]}),
            )
            .unwrap();

        let chain = store.get_chain("resp_3", 1_000_000).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].response_id, "resp_1");
        assert_eq!(chain[2].response_id, "resp_3");
    }

    #[test]
    fn chain_stops_on_self_reference_loop() {
        let (_dir, store) = tmp_store();
        // 故意讓 resp_1 的 previous 指向自己（資料損壞）。
        store
            .record_exchange(
                &json!({"previous_response_id": "resp_1", "input": "x"}),
                &json!({"id": "resp_1", "output": [{"c": "a1"}]}),
            )
            .unwrap();
        let chain = store.get_chain("resp_1", 1_000_000).unwrap();
        // 只取一筆就停（seen 集合擋住）。
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn chain_respects_token_budget() {
        let (_dir, store) = tmp_store();
        for i in 0..5 {
            let prev = if i == 0 {
                "null".to_string()
            } else {
                format!("resp_{}", i)
            };
            let prev_val = if i == 0 { Value::Null } else { json!(prev) };
            store.record_exchange(
                &json!({"previous_response_id": prev_val, "input": format!("輪次 {i} 的長內容")}),
                &json!({"id": format!("resp_{}", i + 1), "output": [{"c": format!("回覆 {i} 的長內容")}]}),
            ).unwrap();
        }
        // 預算極小 → 只能取最新一筆。
        let chain = store.get_chain("resp_5", 1).unwrap();
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn persist_and_reload_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("hist.db");

        {
            let store = HistoryStore::open_with_cipher(db.clone(), test_cipher()).unwrap();
            store
                .record_exchange(
                    &json!({"input": "嗨"}),
                    &json!({"id": "resp_1", "output": [{"c": "嗨嗨"}]}),
                )
                .unwrap();
        }
        // 重開同一個 db 檔，資料還在。
        let store2 = HistoryStore::open_with_cipher(db, test_cipher()).unwrap();
        let entry = store2.get("resp_1").unwrap().unwrap();
        assert_eq!(entry.output_items[0]["c"], "嗨嗨");
    }

    #[test]
    fn evict_removes_old_entries() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": "舊"}),
                &json!({"id": "old", "output": [{"c": "舊回覆"}]}),
            )
            .unwrap();
        // 手動把 created_at 改成很久以前。
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE response_history SET created_at = ?1 WHERE response_id = ?2",
                params![1i64, "old"],
            )
            .unwrap();
        }
        let deleted = store.evict_older_than_days(1).unwrap();
        assert_eq!(deleted, 1);
        assert!(store.get("old").unwrap().is_none());
    }

    #[test]
    fn estimate_tokens_is_positive() {
        assert!(estimate_items_tokens(&[json!({"a": "hi"})]) >= 1);
        assert_eq!(estimate_items_tokens(&[]), 1);
    }

    #[test]
    fn response_realm_round_trip_supports_provider_switch_detection() {
        let (_dir, store) = tmp_store();
        store.mark_response_route("resp_1", "weikuwu").unwrap();
        assert_eq!(
            store.response_route("resp_1").unwrap().as_deref(),
            Some("weikuwu")
        );
        store
            .mark_response_route("resp_1", "openai-official")
            .unwrap();
        assert_eq!(
            store.response_route("resp_1").unwrap().as_deref(),
            Some("openai-official")
        );
    }

    #[test]
    fn conversation_exchange_seed_counts_only_the_grok_route() {
        let (_dir, store) = tmp_store();
        let request = json!({
            "prompt_cache_key": "shared-session",
            "input": [{"role": "user", "content": "hello"}]
        });
        store
            .record_exchange(&request, &json!({"id": "resp_grok", "output": []}))
            .unwrap();
        store.mark_response_route("resp_grok", "grok-cli").unwrap();
        store
            .record_exchange(&request, &json!({"id": "resp_openai", "output": []}))
            .unwrap();
        store
            .mark_response_route("resp_openai", "openai-official")
            .unwrap();
        let key = conversation_key_from_raw(Some("shared-session")).unwrap();
        assert_eq!(
            store
                .conversation_route_exchange_count(&key, "grok-cli")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conversation_route_exchange_count(&key, "openai-official")
                .unwrap(),
            1
        );
    }

    #[test]
    fn latest_conversation_route_works_without_previous_response_id() {
        let (_dir, store) = tmp_store();
        let request = json!({
            "prompt_cache_key": "thread-app-full-snapshot",
            "input": [{"role": "user", "content": "first"}]
        });
        let response = json!({
            "id": "resp_third_party",
            "output": [{"type": "message", "role": "assistant", "content": "done"}]
        });
        assert!(store.record_exchange(&request, &response).unwrap());
        store
            .mark_response_context("resp_third_party", "grok-cli", Some("grok-4.5"))
            .unwrap();

        let resumed_without_previous = json!({
            "prompt_cache_key": "thread-app-full-snapshot",
            "input": [{"role": "user", "content": "continue"}]
        });
        assert_eq!(
            store
                .latest_conversation_route(&resumed_without_previous)
                .unwrap()
                .as_deref(),
            Some("grok-cli")
        );
    }

    #[test]
    fn session_snapshots_group_by_conversation_and_use_latest_route_model() {
        let (_dir, store) = tmp_store();
        for (response_id, route, model, created_at) in [
            ("resp_1", "weikuwu", "GLM-5.2", 100_i64),
            ("resp_2", "grok-cli", "grok-4.5", 200_i64),
        ] {
            store
                .record_exchange(
                    &json!({
                        "prompt_cache_key": "thread-a",
                        "input": [{"role": "user", "content": response_id}]
                    }),
                    &json!({
                        "id": response_id,
                        "output": [{"role": "assistant", "content": "answer"}],
                        "usage": {
                            "input_tokens": if response_id == "resp_2" { 1_200 } else { 100 },
                            "output_tokens": if response_id == "resp_2" { 34 } else { 10 }
                        }
                    }),
                )
                .unwrap();
            store
                .mark_response_context(response_id, route, Some(model))
                .unwrap();
            store
                .conn
                .lock()
                .execute(
                    "UPDATE response_history SET created_at = ?1 WHERE response_id = ?2",
                    params![created_at, response_id],
                )
                .unwrap();
        }

        let snapshots = store.session_snapshots().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].route_id, "grok-cli");
        assert_eq!(snapshots[0].model.as_deref(), Some("grok-4.5"));
        assert_eq!(snapshots[0].last_activity_at, 200);
        assert_eq!(snapshots[0].payload_bytes, 1_234 * BYTES_PER_TOKEN as u64);
    }

    #[test]
    fn recent_compactions_return_one_conversations_handovers_newest_first() {
        let (_dir, store) = tmp_store();
        // A live conversation key, plus a neighbour that must stay out of it.
        let conversation =
            "codex:01a079b3-33f3-7bb3-a4c9-e60261a5267d:01a079b3-33f3-7bb3-a4c9-e60261a5267d";
        for response in ["resp-1", "resp-2", "resp-3", "resp-4"] {
            store
                .record_exchange_with_conversation_key(
                    &json!({"input": response}),
                    &json!({"id": response, "output": []}),
                    Some(conversation),
                )
                .unwrap();
            store
                .save_compaction(
                    response,
                    vec![json!({"type": "message", "role": "user", "content": response})],
                    vec![json!({"type": "message", "role": "assistant", "content": "kept"})],
                )
                .unwrap();
        }
        store
            .record_exchange_with_conversation_key(
                &json!({"input": "other"}),
                &json!({"id": "resp-other", "output": []}),
                Some("codex:other:other"),
            )
            .unwrap();
        store
            .save_compaction(
                "resp-other",
                vec![json!({"type": "message", "role": "user", "content": "other"})],
                vec![json!({"type": "message", "role": "assistant", "content": "other"})],
            )
            .unwrap();

        let recent = store
            .recent_compactions_for_conversation(conversation, 3)
            .unwrap();
        assert_eq!(recent.len(), 3, "the limit caps the list");
        assert!(
            recent
                .iter()
                .all(|journal| journal.compaction_id != "resp-other"),
            "another conversation's handover must never appear"
        );
        // Newest first: the rows share a timestamp at this resolution, so the
        // insertion order is what has to come back reversed.
        assert_eq!(recent[0].compaction_id, "resp-4");
        assert_eq!(recent[2].compaction_id, "resp-2");
        assert_eq!(
            store
                .recent_compactions_for_conversation("codex:missing:missing", 3)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn latest_compaction_for_conversation_never_leaks_another_session() {
        let (_dir, store) = tmp_store();
        for (conversation, response, marker) in [
            ("session-a", "resp-a", "summary-a"),
            ("session-b", "resp-b", "summary-b"),
        ] {
            store
                .record_exchange_with_conversation_key(
                    &json!({"input": conversation}),
                    &json!({"id": response, "output": []}),
                    Some(conversation),
                )
                .unwrap();
            store
                .save_compaction(
                    response,
                    vec![json!({"type": "message", "role": "user", "content": conversation})],
                    vec![json!({"type": "message", "role": "assistant", "content": marker})],
                )
                .unwrap();
        }

        let a = store
            .latest_compaction_for_conversation("session-a")
            .unwrap()
            .expect("session-a compaction");
        let b = store
            .latest_compaction_for_conversation("session-b")
            .unwrap()
            .expect("session-b compaction");
        assert_eq!(a.compaction_id, "resp-a");
        assert_eq!(b.compaction_id, "resp-b");
        assert!(store
            .latest_compaction_for_conversation("missing")
            .unwrap()
            .is_none());
    }

    #[test]
    fn session_snapshot_fallback_uses_only_latest_payload_not_snapshot_sum() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({
                    "prompt_cache_key": "legacy-thread",
                    "input": "an old complete snapshot repeated many times"
                }),
                &json!({"id": "old", "output": [{"content": "large old output"}]}),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({
                    "prompt_cache_key": "legacy-thread",
                    "input": "latest"
                }),
                &json!({"id": "latest", "output": [{"content": "latest output"}]}),
            )
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE response_history SET created_at = 1 WHERE response_id = 'old'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE response_history SET created_at = 2 WHERE response_id = 'latest'",
                [],
            )
            .unwrap();
        }
        let latest_payload: i64 = store
            .conn
            .lock()
            .query_row(
                "SELECT payload_bytes FROM response_history WHERE response_id = 'latest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let snapshots = store.session_snapshots().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].payload_bytes, latest_payload as u64);
    }

    #[test]
    fn compaction_checkpoint_survives_multiple_descendant_responses() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": "old request"}),
                &json!({"id": "a", "output": [{"role": "assistant", "content": "old answer"}]}),
            )
            .unwrap();
        store
            .save_compaction(
                "a",
                vec![json!({"role": "user", "content": "old request"})],
                vec![json!({"role": "user", "content": "checkpoint summary"})],
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "a", "input": "after one"}),
                &json!({"id": "b", "output": [{"role": "assistant", "content": "answer b"}]}),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "b", "input": "after two"}),
                &json!({"id": "c", "output": [{"role": "assistant", "content": "answer c"}]}),
            )
            .unwrap();

        let replay = store.compacted_replay_chain("c").unwrap().unwrap();
        let items = flatten_chain_items(&replay, &[]);
        let text = serde_json::to_string(&items).unwrap();
        assert!(text.contains("checkpoint summary"));
        assert!(text.contains("after one"));
        assert!(text.contains("after two"));
        assert!(!text.contains("old request"));
        assert!(store.restore_compaction_for_chain("c").unwrap());
        assert!(store.compacted_replay_chain("c").unwrap().is_none());
    }

    #[test]
    fn compacted_replay_resolves_cmp_id_via_response_mapping() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": [{"role": "user", "content": "seed"}]}),
                &json!({
                    "id": "resp_parent",
                    "output": [
                        {"type": "compaction", "id": "cmp_checkpoint", "encrypted_content": "opaque"},
                        {"type": "message", "role": "assistant", "content": "after compact"}
                    ]
                }),
            )
            .unwrap();
        store
            .save_canonical_compaction_with_meta(
                "cmp_checkpoint",
                CompactionJournalKind::Official,
                vec![json!({"role": "user", "content": "seed"})],
                vec![
                    json!({"type": "compaction", "id": "cmp_checkpoint", "encrypted_content": "opaque"}),
                    json!({"type": "message", "role": "assistant", "content": "kept"}),
                ],
                None,
                "official",
                Some("openai-official"),
                Some("OpenAI Official"),
                Some("gpt-5.6-sol"),
                CompactionSaveMeta {
                    source_head_response_id: Some("resp_parent".into()),
                    generation: 1,
                    ..CompactionSaveMeta::default()
                },
            )
            .unwrap();
        store
            .record_exchange(
                &json!({
                    "previous_response_id": "resp_parent",
                    "input": [{"role": "user", "content": "child turn"}]
                }),
                &json!({
                    "id": "resp_child",
                    "output": [{"type": "message", "role": "assistant", "content": "child"}]
                }),
            )
            .unwrap();

        let replay = store
            .compacted_replay_chain("resp_child")
            .unwrap()
            .expect("must resolve cmp_* journal via mapping/output");
        let items = flatten_chain_items(&replay, &[]);
        let text = serde_json::to_string(&items).unwrap();
        assert!(text.contains("kept"));
        assert!(text.contains("child turn"));
        assert!(!text.contains("seed"));
    }

    #[test]
    fn consumed_checkpoint_replay_keeps_only_post_checkpoint_exchange() {
        let (_dir, store) = tmp_store();
        let source = vec![json!({
            "type": "message",
            "role": "user",
            "content": "very large pre-compaction history"
        })];
        store
            .save_canonical_compaction_with_meta(
                "cmp_consumed",
                CompactionJournalKind::Local,
                source.clone(),
                vec![json!({
                    "type": "message",
                    "role": "developer",
                    "content": "canonical checkpoint"
                })],
                None,
                "vellum_canonical",
                Some("grok-cli"),
                Some("Grok Build"),
                Some("grok-4.5"),
                CompactionSaveMeta::default(),
            )
            .unwrap();

        let mut followup_input = source;
        followup_input.push(json!({
            "type": "message",
            "role": "user",
            "content": "implement after checkpoint"
        }));
        store
            .record_exchange(
                &json!({"input": followup_input}),
                &json!({
                    "id": "resp_after_checkpoint",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": "implementation completed"
                    }]
                }),
            )
            .unwrap();
        store
            .bind_consumed_response_compaction("resp_after_checkpoint", "cmp_consumed")
            .unwrap();
        store
            .record_exchange(
                &json!({
                    "previous_response_id": "resp_after_checkpoint",
                    "input": [{"type": "message", "role": "user", "content": "run tests"}]
                }),
                &json!({
                    "id": "resp_child",
                    "output": [{"type": "message", "role": "assistant", "content": "tests pass"}]
                }),
            )
            .unwrap();

        let replay = store.compacted_replay_chain("resp_child").unwrap().unwrap();
        let text = serde_json::to_string(&flatten_chain_items(&replay, &[])).unwrap();
        assert!(text.contains("canonical checkpoint"));
        assert!(text.contains("implement after checkpoint"));
        assert!(text.contains("implementation completed"));
        assert!(text.contains("run tests"));
        assert!(text.contains("tests pass"));
        assert!(!text.contains("very large pre-compaction history"));
    }

    #[test]
    fn legacy_consumed_boundary_without_materialized_prefix_recovers_ancestry() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction_with_meta(
                "cmp_legacy_consumed",
                CompactionJournalKind::Local,
                vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": "large pre-compaction history"
                })],
                vec![json!({
                    "type": "message",
                    "role": "developer",
                    "content": "canonical checkpoint"
                })],
                None,
                "vellum_canonical",
                Some("opencode-zen"),
                Some("OpenCode Zen"),
                Some("deepseek-v4-flash-free"),
                CompactionSaveMeta::default(),
            )
            .unwrap();

        // Old builds persisted the original client request, so neither row
        // contains the materialized canonical prefix even though both are
        // durably marked as consumers of the checkpoint.
        store
            .record_exchange(
                &json!({"input": [{"role": "user", "content": "first follow-up"}]}),
                &json!({
                    "id": "resp_legacy_first",
                    "output": [{"role": "assistant", "content": "first answer"}]
                }),
            )
            .unwrap();
        store
            .bind_consumed_response_compaction("resp_legacy_first", "cmp_legacy_consumed")
            .unwrap();
        store
            .record_exchange(
                &json!({
                    "previous_response_id": "resp_legacy_first",
                    "input": [{"role": "user", "content": "second follow-up"}]
                }),
                &json!({
                    "id": "resp_legacy_second",
                    "output": [{"role": "assistant", "content": "second answer"}]
                }),
            )
            .unwrap();
        store
            .bind_consumed_response_compaction("resp_legacy_second", "cmp_legacy_consumed")
            .unwrap();

        let replay = store
            .compacted_replay_chain("resp_legacy_second")
            .unwrap()
            .unwrap();
        let text = serde_json::to_string(&flatten_chain_items(&replay, &[])).unwrap();
        assert!(text.contains("canonical checkpoint"));
        assert!(text.contains("first follow-up"));
        assert!(text.contains("first answer"));
        assert!(text.contains("second follow-up"));
        assert!(text.contains("second answer"));
        assert!(!text.contains("large pre-compaction history"));
    }

    #[test]
    fn get_chain_detailed_counts_budget_omissions() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": "one long historical payload ".repeat(40)}),
                &json!({"id": "r1", "output": [{"role": "assistant", "content": "a".repeat(80)}]}),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "r1", "input": "two ".repeat(40)}),
                &json!({"id": "r2", "output": [{"role": "assistant", "content": "b".repeat(80)}]}),
            )
            .unwrap();
        let detailed = store.get_chain_detailed("r2", 8).unwrap();
        assert!(!detailed.entries.is_empty());
        if detailed.budget_exhausted {
            assert!(detailed.omitted_entries >= 1);
        }
    }

    #[test]
    fn verify_request_compactions_marks_provisional_verified() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction(
                "cmp_live",
                CompactionJournalKind::Official,
                vec![json!("source")],
                vec![json!({"type": "compaction", "id": "cmp_live"})],
                None,
                "official",
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store.get_compaction("cmp_live").unwrap().unwrap().status,
            CheckpointStatus::Provisional
        );
        let verified = store
            .verify_request_compactions(
                &json!({"input": [{"type": "compaction", "id": "cmp_live"}]}),
                "resp_followup",
            )
            .unwrap();
        assert_eq!(verified, 1);
        let journal = store.get_compaction("cmp_live").unwrap().unwrap();
        assert_eq!(journal.status, CheckpointStatus::Verified);
        assert_eq!(
            journal.verified_followup_response_id.as_deref(),
            Some("resp_followup")
        );
    }

    #[test]
    fn later_compaction_reconstructs_original_source_not_opaque_window() {
        let (_dir, store) = tmp_store();
        store
            .record_exchange(
                &json!({"input": [{"role": "user", "content": "original decision"}]}),
                &json!({"id": "a", "output": [{"role": "assistant", "content": "confirmed"}]}),
            )
            .unwrap();
        store
            .save_canonical_compaction(
                "a",
                CompactionJournalKind::Official,
                vec![
                    json!({"role": "user", "content": "original decision"}),
                    json!({"role": "assistant", "content": "confirmed"}),
                ],
                vec![json!({"type": "compaction", "id": "a", "encrypted_content": "opaque"})],
                None,
                "official",
                Some("openai-official"),
                Some("OpenAI Official"),
                Some("gpt-5.6-sol"),
            )
            .unwrap();
        store
            .record_exchange(
                &json!({"previous_response_id": "a", "input": [{"role": "user", "content": "after"}]}),
                &json!({"id": "b", "output": [{"role": "assistant", "content": "after answer"}]}),
            )
            .unwrap();
        let source = store.compaction_source_for_chain("b").unwrap().unwrap();
        let text = serde_json::to_string(&source).unwrap();
        assert!(text.contains("original decision"));
        assert!(text.contains("after answer"));
        assert!(!text.contains("opaque"));
    }

    #[test]
    fn latest_compaction_exposes_automatic_provider_metadata() {
        let (_dir, store) = tmp_store();
        store
            .save_compaction("manual", vec![json!("old")], vec![json!("summary")])
            .unwrap();
        store
            .save_compaction_with_metadata(
                "automatic",
                vec![json!({"role": "user", "content": "Remember 27"})],
                vec![json!({"role": "user", "content": "summary 27"})],
                "codex_auto",
                Some("Grok Build"),
                Some("grok-4.5"),
            )
            .unwrap();

        let latest = store.latest_compaction().unwrap().unwrap();
        assert_eq!(latest.compaction_id, "automatic");
        assert_eq!(latest.trigger, "codex_auto");
        assert_eq!(latest.producer_provider.as_deref(), Some("Grok Build"));
        assert_eq!(latest.producer_model.as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn v1_local_journal_lazily_migrates_to_local_canonical() {
        let legacy = serde_json::from_value::<CompactionJournal>(json!({
            "head_response_id": "cmp_legacy_local",
            "original_items": [{"role": "user", "content": "source"}],
            "replacement_items": [{"role": "developer", "content": "checkpoint"}],
            "created_at": 1,
            "trigger": "codex_auto",
            "provider": "weikuwu",
            "model": "GLM-5.2"
        }))
        .unwrap();
        let migrated = normalize_compaction_journal(legacy).unwrap();
        assert_eq!(migrated.schema_version, COMPACTION_JOURNAL_SCHEMA_V3);
        assert_eq!(migrated.kind, CompactionJournalKind::Local);
        assert!(migrated.canonical_available);
        assert_eq!(migrated.canonical_items[0]["content"], "checkpoint");
        assert!(!migrated.canonical_sha256.is_empty());
        assert_eq!(migrated.status, CheckpointStatus::Provisional);
        assert_eq!(migrated.generation, 1);
    }

    #[test]
    fn v1_official_source_source_journal_is_not_mislabeled_canonical() {
        let legacy = serde_json::from_value::<CompactionJournal>(json!({
            "head_response_id": "cmp_legacy_official",
            "original_items": [{"role": "user", "content": "source"}],
            "replacement_items": [{"role": "user", "content": "source"}],
            "created_at": 1,
            "trigger": "official",
            "provider": "OpenAI Official",
            "model": "gpt-5.6-sol"
        }))
        .unwrap();
        let migrated = normalize_compaction_journal(legacy).unwrap();
        assert_eq!(migrated.kind, CompactionJournalKind::LegacyRecovered);
        assert!(!migrated.canonical_available);
        assert!(migrated.canonical_items.is_empty());
        assert_eq!(migrated.source_items[0]["content"], "source");
    }

    #[test]
    fn canonical_hash_mismatch_fails_closed() {
        let journal = CompactionJournal {
            engine_id: Some("codex_local_v0_150".to_string()),
            engine_provenance: None,
            route_id: None,
            upstream_model: None,
            tokens_before: None,
            tokens_after: None,
            elapsed_ms: None,
            schema_version: 2,
            compaction_id: "cmp_bad_hash".into(),
            kind: CompactionJournalKind::Official,
            source_items: vec![json!("source")],
            canonical_items: vec![json!("canonical")],
            portable_items: None,
            created_at: 1,
            trigger: "official".into(),
            producer_route: Some("openai-official".into()),
            producer_provider: Some("OpenAI Official".into()),
            producer_model: Some("gpt-5.6-sol".into()),
            canonical_sha256: "not-the-real-hash".into(),
            canonical_available: true,
            realm_fingerprint: None,
            parent_compaction_id: None,
            generation: 1,
            status: CheckpointStatus::Provisional,
            continuation_mode: None,
            continuity_kind: None,
            strategy: None,
            reasoning_context_requested: None,
            reasoning_context_effective: None,
            contains_encrypted_reasoning: false,
            reasoning_continuity_verified: false,
            source_head_response_id: None,
            verified_followup_response_id: None,
            input_tokens: None,
            output_tokens: None,
            dropped_message_count: None,
            checkpoint_schema_version: None,
            source_hash: None,
            checkpoint_hash: None,
            audit: None,
        };
        assert!(normalize_compaction_journal(journal)
            .unwrap_err()
            .to_string()
            .contains("failed hash validation"));
    }

    #[test]
    fn portable_window_update_preserves_canonical_hash() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction(
                "cmp_portable",
                CompactionJournalKind::Official,
                vec![json!("source")],
                vec![json!({"type": "compaction", "id": "cmp_portable", "encrypted_content": "opaque"})],
                None,
                "official",
                Some("openai-official"),
                Some("OpenAI Official"),
                Some("gpt-5.6-sol"),
            )
            .unwrap();
        let before = store.get_compaction("cmp_portable").unwrap().unwrap();
        store
            .update_compaction_portable(
                "cmp_portable",
                vec![json!({"role": "developer", "content": "portable"})],
            )
            .unwrap();
        let after = store.get_compaction("cmp_portable").unwrap().unwrap();
        assert_eq!(before.canonical_sha256, after.canonical_sha256);
        assert_eq!(after.portable_items.unwrap()[0]["content"], "portable");
    }

    #[test]
    fn hydration_trims_whole_turn_instead_of_orphaning_tool_output() {
        let chain = vec![HistoryEntry {
            response_id: "a".into(),
            previous_response_id: None,
            input_items: vec![json!({"role": "user", "content": "old"})],
            output_items: vec![
                json!({"type": "function_call", "call_id": "call_1", "name": "read"}),
                json!({"type": "function_call_output", "call_id": "call_1", "output": "large result"}),
            ],
            created_at: 1,
        }];
        let mut body = json!({
            "previous_response_id": "a",
            "input": [{"role": "user", "content": "current"}]
        });
        hydrate_input(&mut body, &chain, 1);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"], "current");
    }

    #[test]
    fn checkpoint_failure_keeps_source_and_allows_rollback() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction_with_meta(
                "cmp_fail",
                CompactionJournalKind::Local,
                vec![json!({"role": "user", "content": "source-window"})],
                vec![json!({"role": "user", "content": "provisional-checkpoint"})],
                None,
                "vellum_canonical",
                Some("or"),
                Some("OpenAI Compatible"),
                Some("qwen"),
                CompactionSaveMeta {
                    generation: 1,
                    continuity_kind: Some("semantic".into()),
                    strategy: Some("vellum_canonical".into()),
                    ..CompactionSaveMeta::default()
                },
            )
            .unwrap();
        assert!(store.mark_compaction_failed("cmp_fail").unwrap());
        let journal = store.get_compaction("cmp_fail").unwrap().unwrap();
        assert_eq!(journal.status, CheckpointStatus::Failed);
        assert_eq!(journal.source_items[0]["content"], "source-window");
        assert_eq!(journal.continuity_kind.as_deref(), Some("semantic"));
        // Source remains available for rollback / retry.
        assert!(!journal.source_items.is_empty());
        assert_eq!(
            store.next_generation_from_parent(Some("cmp_fail")).unwrap(),
            2
        );
    }

    #[test]
    fn parent_generation_comes_from_consumed_checkpoint() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction_with_meta(
                "cmp_parent",
                CompactionJournalKind::Local,
                vec![json!({"role": "user", "content": "a"})],
                vec![json!({"role": "user", "content": "parent"})],
                None,
                "vellum_canonical",
                None,
                None,
                None,
                CompactionSaveMeta {
                    generation: 3,
                    continuity_kind: Some("semantic".into()),
                    strategy: Some("vellum_canonical".into()),
                    ..CompactionSaveMeta::default()
                },
            )
            .unwrap();
        assert_eq!(
            store
                .next_generation_from_parent(Some("cmp_parent"))
                .unwrap(),
            4
        );
        assert_eq!(store.next_generation_from_parent(None).unwrap(), 1);
    }

    #[test]
    fn desktop_history_round_trips_v2_audit_metadata() {
        let (_dir, store) = tmp_store();
        let audit = vellum_proxy_runtime::history::CanonicalAuditRecord {
            schema_version: 2,
            source_hash: "src-hash-abc".into(),
            checkpoint_hash: "checkpoint-hash-def".into(),
            source_item_count: 12,
            source_tokens: 4096,
            checkpoint_tokens: 512,
            source_model_visible_tokens: Some(4096),
            summary_model_visible_tokens: Some(256),
            tail_model_visible_tokens: Some(200),
            replacement_model_visible_tokens: Some(456),
            replacement_durable_tokens: Some(512),
            model_visible_compression_ratio: Some(0.1113),
            semantic_claim_count: 10,
            grounded_claim_count: 8,
            rejected_claim_count: 2,
            prior_checkpoint_used: true,
            prior_checkpoint_hash: Some("prior-checkpoint-hash".into()),
            repeated_sequences_detected: 3,
            repeated_exchanges_collapsed: 1,
            soft_trimmed_outputs: 4,
            hard_cleared_outputs: 0,
            extraction_attempts: 2,
            fallback_used: false,
            fallback_reason: None,
        };
        store
            .save_canonical_compaction_with_meta(
                "cmp_v2_audit",
                CompactionJournalKind::Local,
                vec![json!({"role": "user", "content": "source"})],
                vec![json!({"role": "user", "content": "canonical"})],
                None,
                "vellum_canonical",
                None,
                None,
                None,
                CompactionSaveMeta {
                    generation: 1,
                    continuity_kind: Some("semantic".into()),
                    strategy: Some("canonical_v2".into()),
                    checkpoint_schema_version: Some(2),
                    source_hash: Some("src-hash-abc".into()),
                    checkpoint_hash: Some("checkpoint-hash-def".into()),
                    audit: Some(audit.clone()),
                    ..CompactionSaveMeta::default()
                },
            )
            .unwrap();

        let journal = store.latest_compaction().unwrap().unwrap();
        assert_eq!(journal.strategy.as_deref(), Some("canonical_v2"));
        assert_eq!(journal.continuity_kind.as_deref(), Some("semantic"));
        assert_eq!(journal.checkpoint_schema_version, Some(2));
        assert_eq!(journal.source_hash.as_deref(), Some("src-hash-abc"));
        assert_eq!(
            journal.checkpoint_hash.as_deref(),
            Some("checkpoint-hash-def")
        );
        assert_eq!(journal.audit, Some(audit));
    }

    #[test]
    fn journal_v3_marks_new_checkpoints_provisional() {
        let (_dir, store) = tmp_store();
        store
            .save_canonical_compaction(
                "cmp_v3",
                CompactionJournalKind::Official,
                vec![json!({"role": "user", "content": "source"})],
                vec![json!({"type": "compaction", "id": "cmp_v3", "encrypted_content": "opaque"})],
                None,
                "official",
                Some("openai-official"),
                Some("OpenAI Official"),
                Some("gpt-5.6-sol"),
            )
            .unwrap();
        let journal = store.get_compaction("cmp_v3").unwrap().unwrap();
        assert_eq!(journal.schema_version, COMPACTION_JOURNAL_SCHEMA_V3);
        assert_eq!(journal.status, CheckpointStatus::Provisional);
        assert_eq!(journal.generation, 1);
        assert!(store
            .mark_compaction_verified("cmp_v3", "resp_followup")
            .unwrap());
        let verified = store.get_compaction("cmp_v3").unwrap().unwrap();
        assert_eq!(verified.status, CheckpointStatus::Verified);
        assert_eq!(
            verified.verified_followup_response_id.as_deref(),
            Some("resp_followup")
        );
        assert!(verified.reasoning_continuity_verified);
    }

    #[test]
    fn realm_fingerprint_is_stable_and_opaque() {
        let first = realm_fingerprint(
            "official",
            "https://chatgpt.com/backend-api/codex",
            "chatgpt",
            "acct_123",
        );
        let second = realm_fingerprint(
            "official",
            "https://chatgpt.com/backend-api/codex",
            "chatgpt",
            "acct_123",
        );
        let other = realm_fingerprint(
            "official",
            "https://chatgpt.com/backend-api/codex",
            "chatgpt",
            "acct_other",
        );
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(!first.contains("acct_123"));
    }

    #[test]
    fn catch_history_panic_turns_panic_into_history_unavailable() {
        let err = catch_history_panic(|| -> AppResult<()> {
            panic!("journal lock exploded");
        })
        .unwrap_err();
        match err {
            AppError::HistoryUnavailable(message) => {
                assert!(message.contains("journal lock exploded"), "{message}");
            }
            other => panic!("expected HistoryUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn catch_history_panic_passes_through_ok_and_err() {
        assert_eq!(catch_history_panic(|| Ok(7)).unwrap(), 7);
        let err =
            catch_history_panic(|| -> AppResult<()> { Err(AppError::Message("ordinary".into())) })
                .unwrap_err();
        assert!(matches!(err, AppError::Message(_)));
    }
}
