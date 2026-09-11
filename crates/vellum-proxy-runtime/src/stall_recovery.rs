//! Task-stall recovery copy. No benchmark answers.

use crate::task_stall::TaskStallState;

pub fn level_one_message(state: &TaskStallState) -> String {
    format!(
        "The recent tool results have not produced a new workspace, verification, \
user-instruction, or independent evidence state.\n\n\
Observed {since} completed tool result(s) since last genuine progress.\n\n\
Do not repeat already-observed transcript/session searches unchanged.\n\
Your next step must change execution strategy: modify the workspace, \
run a verification that can distinguish hypotheses, query a genuinely new \
independent source, or explicitly state what user information is missing.",
        since = state.tool_results_since_progress
    )
}

pub fn level_two_message() -> String {
    "Previous recovery guidance did not produce material progress.\n\
Do not continue the same investigation pattern.\n\
Switch to task execution or explicitly report the missing prerequisite."
        .to_string()
}

/// Eval-only finalization copy for the turn where tools are deliberately
/// removed. It must not instruct the model to perform an impossible action.
pub fn bounded_finalization_message() -> String {
    "No further tool calls are available for this bounded finalization turn.\n\
Do not continue investigation. If the requested workspace result is already complete, \
finish with a concise result summary. Otherwise state the exact blocker or missing requirement."
        .to_string()
}
