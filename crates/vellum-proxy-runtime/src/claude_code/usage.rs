//! Verified against Desktop's bundled Claude Code 2.1.299 (see PR evidence).
use axum::http::HeaderMap;
use std::time::Instant;

/// Numeric observations only: never retain raw header values in diagnostics.
#[derive(Clone, Copy, PartialEq)]
pub struct UsageObservation {
    /// Captured before dispatch, so scheduling/lock waits cannot reorder data.
    pub observed_at: Instant,
    pub five_hour: Option<UsageWindowObservation>,
    pub seven_day: Option<UsageWindowObservation>,
}
#[derive(Clone, Copy, PartialEq)]
pub struct UsageWindowObservation {
    pub utilization: f64,
    pub resets_at: i64,
}
impl Default for UsageObservation {
    fn default() -> Self {
        Self {
            observed_at: Instant::now(),
            five_hour: None,
            seven_day: None,
        }
    }
}
impl UsageObservation {
    pub(super) fn from_headers(headers: &HeaderMap) -> Self {
        let window = |used: &str, reset: &str| {
            // Reject ambiguous duplicates and incomplete windows. Missing data
            // must never manufacture a fresh zero or erase the API snapshot.
            if headers.get_all(used).iter().count() != 1
                || headers.get_all(reset).iter().count() != 1
            {
                return None;
            }
            let fraction = headers.get(used)?.to_str().ok()?.parse::<f64>().ok()?;
            let seconds = headers.get(reset)?.to_str().ok()?.parse::<i64>().ok()?;
            if !fraction.is_finite()
                || fraction < 0.0
                || seconds <= 0
                || chrono::DateTime::from_timestamp(seconds, 0).is_none()
            {
                return None;
            }
            Some(UsageWindowObservation {
                // The bundled client documents legitimate values above one.
                // Gates use percent remaining and saturate at exhaustion.
                utilization: fraction.min(1.0) * 100.0,
                resets_at: seconds,
            })
        };
        Self {
            observed_at: Instant::now(),
            five_hour: window(
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-5h-reset",
            ),
            seven_day: window(
                "anthropic-ratelimit-unified-7d-utilization",
                "anthropic-ratelimit-unified-7d-reset",
            ),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }
}
/// Token counts as Anthropic reports them: `input_tokens` excludes cache reads
/// and cache writes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}
impl TokenUsage {
    /// Later events repeat or grow the counts, so a present field replaces.
    fn merge(&mut self, usage: &serde_json::Value) {
        for (target, key) in [
            (&mut self.input_tokens, "input_tokens"),
            (&mut self.output_tokens, "output_tokens"),
            (&mut self.cache_read_input_tokens, "cache_read_input_tokens"),
            (
                &mut self.cache_creation_input_tokens,
                "cache_creation_input_tokens",
            ),
        ] {
            if let Some(value) = usage.get(key).and_then(serde_json::Value::as_u64) {
                *target = value;
            }
        }
    }
}

/// Longest SSE line or JSON body inspected. Usage events are small; a longer
/// line is a content delta and is skipped without buffering.
const MAX_INSPECTED: usize = 1024 * 1024;

/// Reads usage and the error type out of a response as it streams past,
/// without holding the body. Compressed or unknown bodies are not inspected.
pub(super) struct ResponseInspector {
    mode: InspectMode,
    buffer: Vec<u8>,
    overflow: bool,
    pub usage: TokenUsage,
    pub error_type: Option<String>,
}
#[derive(PartialEq)]
enum InspectMode {
    EventStream,
    Json,
    Skip,
}
impl ResponseInspector {
    pub(super) fn new(headers: &HeaderMap) -> Self {
        let encoded = headers
            .get(axum::http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.eq_ignore_ascii_case("identity"));
        let content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mode = if encoded {
            InspectMode::Skip
        } else if content_type.starts_with("text/event-stream") {
            InspectMode::EventStream
        } else if content_type.starts_with("application/json") {
            InspectMode::Json
        } else {
            InspectMode::Skip
        };
        Self {
            mode,
            buffer: Vec::new(),
            overflow: false,
            usage: TokenUsage::default(),
            error_type: None,
        }
    }
    pub(super) fn feed(&mut self, mut chunk: &[u8]) {
        match self.mode {
            InspectMode::Skip => {}
            InspectMode::Json => {
                if self.buffer.len() + chunk.len() > MAX_INSPECTED {
                    self.mode = InspectMode::Skip;
                    self.buffer = Vec::new();
                } else {
                    self.buffer.extend_from_slice(chunk);
                }
            }
            InspectMode::EventStream => {
                while let Some(end) = chunk.iter().position(|&b| b == b'\n') {
                    if !self.overflow {
                        self.buffer.extend_from_slice(&chunk[..end]);
                        let line = std::mem::take(&mut self.buffer);
                        self.event_line(&line);
                    }
                    self.buffer.clear();
                    self.overflow = false;
                    chunk = &chunk[end + 1..];
                }
                if !self.overflow {
                    if self.buffer.len() + chunk.len() > MAX_INSPECTED {
                        self.overflow = true;
                        self.buffer = Vec::new();
                    } else {
                        self.buffer.extend_from_slice(chunk);
                    }
                }
            }
        }
    }
    /// Completes a JSON body; a stream's last line always ends in a newline.
    pub(super) fn finish(&mut self) {
        if self.mode == InspectMode::Json {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.buffer) {
                if let Some(usage) = value.get("usage") {
                    self.usage.merge(usage);
                } else if value.get("type").and_then(|t| t.as_str()) != Some("error") {
                    // count_tokens answers with a bare input_tokens.
                    self.usage.merge(&value);
                }
                self.error_type = error_type(&value);
            }
        }
        self.mode = InspectMode::Skip;
        self.buffer = Vec::new();
    }
    fn event_line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        // Cheap prefilter: content deltas are the bulk of a stream.
        let interesting = |needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
        if !interesting(b"\"usage\"") && !interesting(b"\"error\"") {
            return;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(data.trim_ascii()) else {
            return;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage.merge(usage);
                }
            }
            Some("message_delta") => {
                if let Some(usage) = value.get("usage") {
                    self.usage.merge(usage);
                }
            }
            Some("error") => self.error_type = error_type(&value),
            _ => {}
        }
    }
}
fn error_type(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/error/type")
        .and_then(|t| t.as_str())
        .filter(|t| t.len() <= 64 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspector_reads_split_stream_usage_and_json_errors() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "text/event-stream".parse().unwrap(),
        );
        let mut stream = ResponseInspector::new(&headers);
        let text = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":30,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"\\\"usage\\\" is text\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":44}}\n\n",
        );
        for piece in text.as_bytes().chunks(7) {
            stream.feed(piece);
        }
        stream.finish();
        assert_eq!(
            stream.usage,
            TokenUsage {
                input_tokens: 12,
                output_tokens: 44,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 30,
            }
        );
        let mut long = ResponseInspector::new(&headers);
        long.feed(b"data: ");
        long.feed(&vec![b'x'; MAX_INSPECTED + 1]);
        long.feed(b"\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n");
        assert_eq!(long.usage.output_tokens, 3);

        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let mut error = ResponseInspector::new(&headers);
        error.feed(br#"{"type":"error","error":{"type":"rate_limit_error","message":"x"}}"#);
        error.finish();
        assert_eq!(error.error_type.as_deref(), Some("rate_limit_error"));
        assert_eq!(error.usage, TokenUsage::default());
        let mut count = ResponseInspector::new(&headers);
        count.feed(br#"{"input_tokens":10}"#);
        count.finish();
        assert_eq!(count.usage.input_tokens, 10);

        headers.insert(
            axum::http::header::CONTENT_ENCODING,
            "gzip".parse().unwrap(),
        );
        let mut compressed = ResponseInspector::new(&headers);
        compressed.feed(br#"{"usage":{"input_tokens":10}}"#);
        compressed.finish();
        assert_eq!(compressed.usage, TokenUsage::default());
    }
    #[test]
    fn verified_fraction_and_epoch_units_and_invalid_windows() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.85".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "1900000000".parse().unwrap(),
        );
        let w = UsageObservation::from_headers(&h).five_hour.unwrap();
        assert_eq!(w.utilization, 85.0);
        assert_eq!(w.resets_at, 1900000000);
        for (input, expected) in [
            ("1.2", Some(100.0)),
            ("NaN", None),
            ("inf", None),
            ("-0.1", None),
            ("bad", None),
        ] {
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                input.parse().unwrap(),
            );
            assert_eq!(
                UsageObservation::from_headers(&h)
                    .five_hour
                    .map(|w| w.utilization),
                expected
            );
        }
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.5".parse().unwrap(),
        );
        for reset in ["2026-10-20T00:00:00Z", "-1", "9223372036854775807", "bad"] {
            h.insert(
                "anthropic-ratelimit-unified-5h-reset",
                reset.parse().unwrap(),
            );
            assert!(UsageObservation::from_headers(&h).is_empty());
        }
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "1900000000".parse().unwrap(),
        );
        h.append(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.2".parse().unwrap(),
        );
        assert!(UsageObservation::from_headers(&h).is_empty());
        assert!(UsageObservation::from_headers(&HeaderMap::new()).is_empty());
    }
}
