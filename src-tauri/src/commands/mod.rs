//! Tauri 指令層。
//!
//! 規則：指令只負責搬資料與轉錯誤，不放商業邏輯。
//! 邏輯放在各自的模組裡，才測得動（指令本身需要 Tauri runtime 才能跑）。

pub mod budget;
pub mod codex_oauth;
pub mod grok_accounts;
pub mod overview;
pub mod probe;
pub mod proxy;
pub mod review;
pub mod runtime;
pub mod subagent;
pub mod web_search;

// 必須是 glob：`#[tauri::command]` 會在函式旁邊生成隱藏項目（`__cmd__*`），
// `generate_handler!` 兩者都要找得到。具名 re-export 只帶走函式，會編不過。
pub use budget::*;
pub use codex_oauth::*;
pub use grok_accounts::*;
pub use overview::*;
pub use probe::*;
pub use proxy::*;
pub use review::*;
pub use runtime::*;
pub use subagent::*;
pub use web_search::*;
