//! 重複工具呼叫的斷路器。
//!
//! 2026-08-07 一個 SuperGrok 帳號在六小時內把整週額度打光。事後查 Codex 的
//! session 紀錄：1,095 次工具呼叫裡有 911 次（83%）在執行前就被拒絕——
//!
//! ```text
//! failed to parse function arguments: invalid type: floating point `120000.0`, expected u64
//! ```
//!
//! 模型收到錯誤、用一模一樣的參數重試、再被拒絕。同一個 `git push` 指令送了
//! 282 次，`cargo test` 送了 184 次，中位間隔 4 秒，指令一次都沒真的跑過。
//! 每一輪都要重送當時 20 萬 token 的 context。
//!
//! [`crate::proxy`] 會正規化那個特定的浮點數問題，但那只堵住已知的一種。這裡
//! 是通用的保險：**任何**讓模型卡在同一個呼叫上的原因，都在花錢之前先停下來。
//!
//! 判斷放在請求路徑而不是回應路徑，因為要擋的正是那次推論的費用——等模型回
//! 完再偵測，錢已經付掉了。

use serde_json::Value;

/// 連續幾次一模一樣的工具呼叫就判定為卡住。
///
/// 正常的工作不會連續五次送出 byte 完全相同的呼叫：真的在輪詢的話，指令會變
/// （不同路徑、不同計數），或結果會變。五次相同的呼叫**配上**五次相同的結果，
/// 代表這個迴圈不會自己結束。
pub const DEFAULT_REPEAT_LIMIT: usize = 5;

/// 卡住的迴圈長什麼樣子，用來寫錯誤訊息與測試。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedCallRun {
    /// 重複的工具名稱。
    pub name: String,
    /// 連續重複了幾次。
    pub count: usize,
    /// 這些呼叫拿到的結果（都相同），截斷後用於錯誤訊息。
    pub last_output: Option<String>,
}

fn is_function_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "local_shell_call" | "custom_tool_call")
    )
}

/// Responses-wire results are typed; Chat-wire results are `role: "tool"`
/// messages with no `type` at all, so both shapes need matching.
fn is_function_call_output(item: &Value) -> bool {
    crate::compaction::is_tool_output(item)
        || item.get("role").and_then(Value::as_str) == Some("tool")
}

/// 呼叫的識別：名稱 + 參數。兩者都相同才算「同一個呼叫」。
fn call_identity(item: &Value) -> Option<(String, String)> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = match item.get("arguments").or_else(|| item.get("input")) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => serde_json::to_string(other).ok()?,
        None => String::new(),
    };
    Some((name, arguments))
}

/// 一次工具往返：呼叫的識別（名稱 + 參數）與它拿到的結果。
type CallRoundTrip = (Option<(String, String)>, Option<String>);

fn output_text(item: &Value) -> Option<String> {
    match item.get("output").or_else(|| item.get("content")) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => serde_json::to_string(other).ok(),
        None => None,
    }
}

/// 從對話尾端往回數，看最後連續幾次是同一個工具呼叫配同一個結果。
///
/// 只看尾端：中間出現過的重複代表模型後來脫困了，不該擋。
pub fn trailing_repeated_call_run(items: &[Value]) -> Option<RepeatedCallRun> {
    // 收集尾端的 (呼叫, 結果) 配對，由新到舊。
    let mut pairs: Vec<CallRoundTrip> = Vec::new();
    let mut index = items.len();
    while index > 0 {
        index -= 1;
        let item = &items[index];
        if is_function_call_output(item) {
            let output = output_text(item);
            // 對應的呼叫應該就在前一個位置。
            if index == 0 {
                break;
            }
            index -= 1;
            let call = &items[index];
            if !is_function_call(call) {
                break;
            }
            pairs.push((call_identity(call), output));
            continue;
        }
        // 尾端還沒送出結果的那次呼叫先跳過，它還沒有結果可比。
        if is_function_call(item) && pairs.is_empty() {
            continue;
        }
        break;
    }

    let first = pairs.first()?;
    let identity = first.0.clone()?;
    let mut count = 0usize;
    for (call, output) in &pairs {
        if call.as_ref() == Some(&identity) && output == &first.1 {
            count += 1;
        } else {
            break;
        }
    }
    if count == 0 {
        return None;
    }
    Some(RepeatedCallRun {
        name: identity.0,
        count,
        last_output: first.1.clone(),
    })
}

/// 這個請求該不該在打上游之前擋下來。
///
/// `limit` 為 0 表示關閉。
pub fn should_fail_closed(request: &Value, limit: usize) -> Option<RepeatedCallRun> {
    if limit == 0 {
        return None;
    }
    let items = ["input", "messages"]
        .into_iter()
        .find_map(|key| request.get(key).and_then(Value::as_array))?;
    let run = trailing_repeated_call_run(items)?;
    (run.count >= limit).then_some(run)
}

/// `VELLUM_REPEAT_CALL_LIMIT` 可調；`0` 關閉這道保險。
pub fn repeat_limit_from_env() -> usize {
    std::env::var("VELLUM_REPEAT_CALL_LIMIT")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_REPEAT_LIMIT)
}

impl RepeatedCallRun {
    /// 給使用者看的說明。要講清楚「為什麼停」跟「該去看什麼」，因為這個錯誤
    /// 是 Vellum 主動製造的，不是上游回的。
    pub fn message(&self) -> String {
        let detail = self
            .last_output
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| {
                let mut snippet: String = text.chars().take(300).collect();
                if text.chars().count() > 300 {
                    snippet.push('…');
                }
                format!(" Every attempt returned: {snippet}")
            })
            .unwrap_or_default();
        format!(
            "Vellum stopped this turn: the model sent the same `{}` tool call {} times in a row \
             and got the same result each time, so it is not making progress.{} \
             Nothing was sent upstream, so this turn cost nothing. Fix the underlying tool error \
             or start a new turn; set VELLUM_REPEAT_CALL_LIMIT=0 to disable this guard.",
            self.name, self.count, detail
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: &str) -> Value {
        json!({"type": "function_call", "call_id": "c", "name": name, "arguments": arguments})
    }
    fn output(text: &str) -> Value {
        json!({"type": "function_call_output", "call_id": "c", "output": text})
    }

    /// 真實案例：Grok 送出 `timeout_ms: 120000.0`，Codex 在執行前就拒絕，
    /// 模型用同樣的參數無限重試。
    fn stuck_conversation(repeats: usize) -> Value {
        let mut input = vec![json!({"role": "user", "content": "push the branch"})];
        for _ in 0..repeats {
            input.push(call(
                "shell_command",
                r#"{"command":"git push -u origin HEAD","timeout_ms":120000.0}"#,
            ));
            input.push(output(
                "failed to parse function arguments: invalid type: floating point `120000.0`, expected u64",
            ));
        }
        json!({"input": input})
    }

    #[test]
    fn five_identical_calls_with_identical_results_fail_closed() {
        let request = stuck_conversation(5);
        let run = should_fail_closed(&request, DEFAULT_REPEAT_LIMIT).expect("must trip");
        assert_eq!(run.name, "shell_command");
        assert_eq!(run.count, 5);
        assert!(run.message().contains("not making progress"));
        assert!(run.message().contains("cost nothing"));
    }

    #[test]
    fn four_repeats_are_still_allowed_through() {
        let request = stuck_conversation(4);
        assert!(should_fail_closed(&request, DEFAULT_REPEAT_LIMIT).is_none());
    }

    /* 指令會變的輪詢是正常工作，不能擋。 */
    #[test]
    fn a_loop_whose_command_changes_is_not_stuck() {
        let mut input = vec![json!({"role": "user", "content": "check each file"})];
        for turn in 0..10 {
            input.push(call(
                "shell_command",
                &format!(r#"{{"command":"cat file_{turn}.txt"}}"#),
            ));
            input.push(output("contents"));
        }
        assert!(should_fail_closed(&json!({"input": input}), DEFAULT_REPEAT_LIMIT).is_none());
    }

    /* 同一個指令但結果會變 —— 那是有進展的輪詢（等 build、等容器起來）。 */
    #[test]
    fn the_same_command_returning_different_results_is_progress() {
        let mut input = vec![json!({"role": "user", "content": "wait for the container"})];
        for turn in 0..10 {
            input.push(call("shell_command", r#"{"command":"docker ps"}"#));
            input.push(output(&format!("status after {turn}s")));
        }
        assert!(should_fail_closed(&json!({"input": input}), DEFAULT_REPEAT_LIMIT).is_none());
    }

    /* 早期卡過但後來脫困，不該再擋——只看尾端。 */
    #[test]
    fn an_earlier_stuck_run_that_recovered_does_not_trip() {
        let mut input = stuck_conversation(8)["input"].as_array().unwrap().clone();
        input.push(call(
            "shell_command",
            r#"{"command":"git push","timeout_ms":120000}"#,
        ));
        input.push(output("Everything up-to-date"));
        assert!(should_fail_closed(&json!({"input": input}), DEFAULT_REPEAT_LIMIT).is_none());
    }

    /* 尾端那次呼叫還沒有結果（正在等上游），不能因此少算或誤判。 */
    #[test]
    fn a_pending_call_without_its_output_does_not_break_counting() {
        let mut input = stuck_conversation(5)["input"].as_array().unwrap().clone();
        input.push(call(
            "shell_command",
            r#"{"command":"git push -u origin HEAD","timeout_ms":120000.0}"#,
        ));
        let run = should_fail_closed(&json!({"input": input}), DEFAULT_REPEAT_LIMIT)
            .expect("the completed repeats still count");
        assert_eq!(run.count, 5);
    }

    #[test]
    fn chat_wire_conversations_are_covered_too() {
        let mut messages = vec![json!({"role": "user", "content": "go"})];
        for _ in 0..5 {
            messages.push(call("shell_command", r#"{"command":"x","timeout_ms":1.0}"#));
            messages.push(json!({"role": "tool", "content": "bad argument"}));
        }
        assert!(should_fail_closed(&json!({"messages": messages}), DEFAULT_REPEAT_LIMIT).is_some());
    }

    #[test]
    fn the_guard_can_be_disabled() {
        assert!(should_fail_closed(&stuck_conversation(50), 0).is_none());
    }

    #[test]
    fn a_conversation_with_no_tool_calls_is_untouched() {
        let request = json!({"input": [{"role": "user", "content": "hello"}]});
        assert!(should_fail_closed(&request, DEFAULT_REPEAT_LIMIT).is_none());
    }
}
