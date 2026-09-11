//! SSE（Server-Sent Events）解析工具。
//!
//! 從 cc-switch 的 `proxy/sse.rs` 移植（經 Desktop `src-tauri/src/sse.rs`），
//! 現在由 Desktop 與 headless runtime 共用（M5 extraction）。
//! 三個純函式，不碰磁碟、不綁任何 enum variant：
//!   - `strip_sse_field`：拆 `data: ` / `event:` 欄位，容忍有無空白。
//!   - `take_sse_block`：從緩衝取出一段 SSE 事件（LF 或 CRLF 空行分隔）。
//!   - `append_utf8_safe`：把位元組安全接進 UTF-8 字串，跨 chunk 邊界也能拼回。

/// 從一行 SSE 取出某欄位的值，容忍 `field: value` 與 `field:value` 兩種空白寫法。
#[inline]
pub fn strip_sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.strip_prefix(&format!("{field}: "))
        .or_else(|| line.strip_prefix(&format!("{field}:")))
}

/// Third-party SSE single-event cap. Official native streams do not use this.
pub const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Third-party parser pending-buffer cap. Official native streams do not use this.
pub const MAX_PARSER_PENDING_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// Why a third-party SSE parser refused to keep reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLimitError {
    PendingBuffer { bytes: usize, limit: usize },
    Event { bytes: usize, limit: usize },
}

impl SseLimitError {
    pub fn name(&self) -> &'static str {
        match self {
            Self::PendingBuffer { .. } => "parser pending buffer",
            Self::Event { .. } => "third-party SSE event",
        }
    }

    pub fn limit(&self) -> usize {
        match self {
            Self::PendingBuffer { limit, .. } | Self::Event { limit, .. } => *limit,
        }
    }

    pub fn bytes(&self) -> usize {
        match self {
            Self::PendingBuffer { bytes, .. } | Self::Event { bytes, .. } => *bytes,
        }
    }
}

/// 從 `buffer` 開頭取出一個 SSE 事件區塊（以 LF 或 CRLF 空行分隔），
/// 並把該區塊從緩衝移除。沒有完整區塊時回 `None`。
///
/// 同時處理 `\r\n\r\n` 與 `\n\n`：取最早出現的分隔。
pub fn take_sse_block(buffer: &mut String) -> Option<String> {
    let mut best: Option<(usize, usize)> = None;

    for (delimiter, len) in [("\r\n\r\n", 4usize), ("\n\n", 2usize)] {
        if let Some(pos) = buffer.find(delimiter) {
            if best.is_none_or(|(best_pos, _)| pos < best_pos) {
                best = Some((pos, len));
            }
        }
    }

    let (pos, len) = best?;
    let block = buffer[..pos].to_string();
    buffer.drain(..pos + len);
    Some(block)
}

/// Third-party bounded variant of [`take_sse_block`].
///
/// A complete event larger than [`MAX_SSE_EVENT_BYTES`], or an incomplete
/// pending buffer larger than [`MAX_PARSER_PENDING_BUFFER_BYTES`], is a
/// protocol error. Official native streams must keep using [`take_sse_block`].
pub fn take_limited_sse_block(buffer: &mut String) -> Result<Option<String>, SseLimitError> {
    match take_sse_block(buffer) {
        Some(block) if block.len() > MAX_SSE_EVENT_BYTES => Err(SseLimitError::Event {
            bytes: block.len(),
            limit: MAX_SSE_EVENT_BYTES,
        }),
        Some(block) => Ok(Some(block)),
        None if buffer.len() > MAX_PARSER_PENDING_BUFFER_BYTES => {
            Err(SseLimitError::PendingBuffer {
                bytes: buffer.len(),
                limit: MAX_PARSER_PENDING_BUFFER_BYTES,
            })
        }
        None => Ok(None),
    }
}

/// 把原始位元組安全接進 UTF-8 `buffer`，正確處理跨 chunk 邊界的多位元組字元。
///
/// `remainder` 累積上一個 chunk 末端不完整的 UTF-8 序列（正常最多 3 位元組）。
/// 每次呼叫把 remainder 前接到 `new_bytes`，把最長的有效 UTF-8 前綴接進 `buffer`，
/// 末端不完整的位元組再存回 `remainder` 供下次使用。
///
/// 防護：若 `remainder` 超過 3 位元組（合法 UTF-8 不可能），直接 lossy 清掉重來。
pub fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, new_bytes: &[u8]) {
    // 把前一次的尾端位元組前接上來。
    let (owned, bytes): (Option<Vec<u8>>, &[u8]) = if remainder.is_empty() {
        (None, new_bytes)
    } else {
        if remainder.len() > 3 {
            buffer.push_str(&String::from_utf8_lossy(remainder));
            remainder.clear();
            (None, new_bytes)
        } else {
            let mut combined = std::mem::take(remainder);
            combined.extend_from_slice(new_bytes);
            (Some(combined), &[])
        }
    };
    let input = owned.as_deref().unwrap_or(bytes);

    // 解碼迴圈：吃掉所有有效 UTF-8 與真正的無效位元組，只在尾端留不完整序列。
    let mut pos = 0;
    loop {
        match std::str::from_utf8(&input[pos..]) {
            Ok(s) => {
                buffer.push_str(s);
                return;
            }
            Err(e) => {
                let valid_up_to = pos + e.valid_up_to();
                let valid_slice = &input[pos..valid_up_to];
                match std::str::from_utf8(valid_slice) {
                    Ok(valid) => buffer.push_str(valid),
                    Err(_) => buffer.push_str(&String::from_utf8_lossy(valid_slice)),
                }
                if let Some(invalid_len) = e.error_len() {
                    // 真正無效的位元組——吐 U+FFFD 後繼續。
                    buffer.push('\u{FFFD}');
                    pos = valid_up_to + invalid_len;
                } else {
                    // 尾端不完整——留給下次。
                    *remainder = input[valid_up_to..].to_vec();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_utf8_safe, strip_sse_field, take_limited_sse_block, take_sse_block, SseLimitError,
        MAX_PARSER_PENDING_BUFFER_BYTES, MAX_SSE_EVENT_BYTES,
    };

    #[test]
    fn strip_sse_field_accepts_optional_space() {
        assert_eq!(
            strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("event: message_start", "event"),
            Some("message_start")
        );
        assert_eq!(
            strip_sse_field("event:message_start", "event"),
            Some("message_start")
        );
        assert_eq!(strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn take_sse_block_supports_lf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\n\nrest".to_string();
        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    #[test]
    fn take_sse_block_supports_crlf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\r\n\r\nrest".to_string();
        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    #[test]
    fn take_sse_block_returns_none_when_incomplete() {
        let mut buffer = "data: {\"ok\":true}".to_string();
        assert!(take_sse_block(&mut buffer).is_none());
        // 沒取走任何東西。
        assert_eq!(buffer, "data: {\"ok\":true}");
    }

    #[test]
    fn take_sse_block_takes_earliest_of_crlf_and_lf() {
        // LF 出現在 CRLF 模式之前 -> 取最早。
        let mut buffer = "a:1\n\nb:2\r\n\r\n".to_string();
        assert_eq!(take_sse_block(&mut buffer).as_deref(), Some("a:1"));
        assert_eq!(take_sse_block(&mut buffer).as_deref(), Some("b:2"));
    }

    #[test]
    fn take_limited_sse_block_rejects_an_oversized_event() {
        let mut buffer = format!("data: {}\n\n", "x".repeat(MAX_SSE_EVENT_BYTES + 1));
        let error = take_limited_sse_block(&mut buffer).unwrap_err();
        assert!(matches!(
            error,
            SseLimitError::Event {
                limit: MAX_SSE_EVENT_BYTES,
                ..
            }
        ));
    }

    #[test]
    fn take_limited_sse_block_rejects_an_oversized_pending_buffer() {
        let mut buffer = "x".repeat(MAX_PARSER_PENDING_BUFFER_BYTES + 1);
        let error = take_limited_sse_block(&mut buffer).unwrap_err();
        assert!(matches!(
            error,
            SseLimitError::PendingBuffer {
                limit: MAX_PARSER_PENDING_BUFFER_BYTES,
                ..
            }
        ));
    }

    #[test]
    fn append_ascii_in_one_chunk() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hello");
        assert_eq!(buf, "hello");
        assert!(rem.is_empty());
    }

    #[test]
    fn ascii_split_across_chunks() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hel");
        append_utf8_safe(&mut buf, &mut rem, b"lo");
        assert_eq!(buf, "hello");
        assert!(rem.is_empty());
    }

    #[test]
    fn multibyte_char_split_at_boundary() {
        // "hi你" = 68 69 E4 BD A0
        let all = "hi你".as_bytes();
        assert_eq!(all.len(), 5);

        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &all[..3]);
        assert_eq!(buf, "hi");
        assert_eq!(rem.len(), 1);

        append_utf8_safe(&mut buf, &mut rem, &all[3..]);
        assert_eq!(buf, "hi你");
        assert!(rem.is_empty());
    }

    #[test]
    fn multiple_split_characters_in_sequence() {
        let text = "你好";
        let bytes = text.as_bytes(); // E4 BD A0 E5 A5 BD

        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &bytes[..4]);
        assert_eq!(buf, "你");
        assert_eq!(rem.len(), 1);

        append_utf8_safe(&mut buf, &mut rem, &bytes[4..]);
        assert_eq!(buf, "你好");
        assert!(rem.is_empty());
    }

    #[test]
    fn empty_chunks_are_harmless() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "");
        assert!(rem.is_empty());
        append_utf8_safe(&mut buf, &mut rem, b"ok");
        assert_eq!(buf, "ok");
        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "ok");
    }

    #[test]
    fn sse_json_with_chinese_split_at_boundary() {
        let json_line = "data: {\"text\":\"你好\"}\n\n";
        let bytes = json_line.as_bytes();
        let ni_start = bytes.windows(3).position(|w| w == "你".as_bytes()).unwrap();
        let split_point = ni_start + 1;

        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &bytes[..split_point]);
        append_utf8_safe(&mut buf, &mut rem, &bytes[split_point..]);

        assert_eq!(buf, json_line);
        assert!(rem.is_empty());

        let data = strip_sse_field(buf.lines().next().unwrap(), "data").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["text"], "你好");
    }

    #[test]
    fn invalid_bytes_flushed_immediately_not_accumulated() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hi\xFFok");
        assert!(
            rem.is_empty(),
            "remainder should be empty after invalid byte"
        );
        assert!(buf.contains("hi"));
        assert!(buf.contains("ok"));
        assert!(buf.contains('\u{FFFD}'));
    }

    #[test]
    fn invalid_byte_in_slow_path_flushed_immediately() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &"你".as_bytes()[..1]);
        assert_eq!(rem.len(), 1);
        append_utf8_safe(&mut buf, &mut rem, b"\xFFworld");
        assert!(rem.is_empty());
        assert!(buf.contains("world"));
    }

    #[test]
    fn defensive_guard_flushes_oversized_remainder() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        rem.extend_from_slice(b"\x80\x80\x80\x80");
        assert_eq!(rem.len(), 4);
        append_utf8_safe(&mut buf, &mut rem, b"hello");
        assert!(rem.is_empty());
        assert!(buf.contains("hello"));
        let count = buf.chars().filter(|&c| c == '\u{FFFD}').count();
        assert_eq!(count, 4);
    }
}
