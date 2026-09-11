//! 指令錯誤。
//!
//! Tauri 指令必須回 `Result<T, E>` 且 `E: Serialize`。
//! 錯誤訊息會直接顯示給使用者，所以寫的是「發生什麼、怎麼修」，
//! 不是 Rust 的內部型別名稱。

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("找不到線路 {0}")]
    RouteNotFound(String),

    /// The model is configured and reaches Codex through the catalog, but the
    /// running proxy routes against the snapshot it took when it started.
    /// Adding, enabling or deleting a Provider is deliberately deferred to the
    /// next activation (the Provider list shows it as 待生效), so this is a
    /// pending change rather than a missing route — and saying so is the whole
    /// difference between an actionable message and an opaque catalog hash.
    #[error("「{provider}」的 {model} 還沒套用到執行中的 Proxy。設定已存檔，重啟 Proxy 後生效。")]
    RouteNotActivated { provider: String, model: String },

    #[error("端點無法連線：{0}")]
    Unreachable(String),

    #[error("{0}")]
    Message(String),

    /// A history-store operation panicked or hit an unrecoverable storage
    /// error. Distinguished from `Message` so callers/UI can tell "history is
    /// having a bad moment" apart from an ordinary validation error, and so a
    /// panic caught at the command boundary (see
    /// `history::catch_history_panic`) never surfaces as an opaque crashed
    /// command with no signal. Other proxy/session functionality is
    /// unaffected — the connection lock does not poison.
    #[error("歷史記錄暫時無法使用：{0}")]
    HistoryUnavailable(String),
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
