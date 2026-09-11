//! Exact translated tool contracts.
//!
//! Issue #6 Phases 2 and 3. The previous translation had two lossy paths:
//!
//! * every unrecognised Codex built-in became
//!   `{"type": "object", "additionalProperties": true}`, which preserves
//!   callability and nothing else — no field names, no constraints, no
//!   mutation semantics;
//! * `apply_patch` lost its freeform Lark grammar and became a generic
//!   `{input: string}` function, so the patch contract stopped looking
//!   authoritative and the model drifted back to shell edits.
//!
//! Every forwarded built-in now resolves to exactly one of: an exact schema, or
//! an explicit `Unsupported`. A mutating tool is never advertised with a
//! permissive schema, and never advertised at all when Vellum cannot dispatch
//! it — a tool the model can call but the runtime cannot execute is worse than
//! an absent tool.

use serde_json::{json, Value};

// The apply_patch envelope parser and namespace-name flattening helper live in
// `crate::adapter`; they are re-exported here so callers keep resolving them
// through the harness tool module they always did.
pub use crate::adapter::{
    flatten_namespace_name, is_apply_patch, normalize_patch_delimiters, validate_patch, FileChange,
    FileChangeKind, PatchSummary, APPLY_PATCH_TOOL_NAME,
};

/// Result of resolving a Codex built-in tool type into something a translated
/// provider can actually be shown.
#[derive(Debug, Clone, PartialEq)]
pub enum BuiltinDecision {
    Exact {
        description: String,
        parameters: Value,
        mutating: bool,
    },
    /// Deliberately not exposed. `reason` is recorded in the harness snapshot
    /// so a diff against the native Sol surface shows the gap explicitly.
    Unsupported { reason: String },
}

impl BuiltinDecision {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Exact { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Unsupported { reason } => Some(reason),
            Self::Exact { .. } => None,
        }
    }
}

fn exact(description: &str, parameters: Value, mutating: bool) -> BuiltinDecision {
    BuiltinDecision::Exact {
        description: description.to_string(),
        parameters,
        mutating,
    }
}

fn unsupported(reason: &str) -> BuiltinDecision {
    BuiltinDecision::Unsupported {
        reason: reason.to_string(),
    }
}

/// Resolve one Codex built-in tool type.
///
/// `shell` keeps `command` as an array because that is what the Codex action
/// carries; flattening it is what destroyed argument boundaries in history.
pub fn builtin_tool(kind: &str) -> BuiltinDecision {
    match kind {
        // The schema and `shell::shell_call_arguments` are one contract: every
        // history item this harness emits must validate here. `command` accepts
        // both forms because both genuinely occur — an argv array from a
        // `local_shell_call` action, and a single script string from a
        // `shell_command`-style action or an older history.
        "shell" | "local_shell" | "shell_command" => exact(
            "Run a command. Prefer the argv array form and pass each argument as its own element, so quoting, spaces, and non-ASCII paths survive exactly; use the single-string form only for genuine shell scripts (pipelines, redirection, builtins). Use this for command execution only — never as a file editor.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "description": "Either the program and its arguments, one array element per argument, or a single shell script string.",
                        "anyOf": [
                            {"type": "array", "items": {"type": "string"}},
                            {"type": "string"}
                        ]
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Absolute working directory for this command."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Hard timeout in milliseconds."
                    },
                    "with_escalated_permissions": {"type": "boolean"},
                    "justification": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            true,
        ),
        "web_search" => exact(
            "Search the web and return ranked results. Read-only.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
            false,
        ),
        "x_search" => exact(
            "Search X/Twitter posts. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            false,
        ),
        "file_search" => exact(
            "Search the indexed files attached to this conversation. Read-only; does not read the working tree.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
            false,
        ),
        // Codex's code interpreter runs inside OpenAI's own sandbox. There is
        // no Vellum runtime behind it, so an exact schema would still produce
        // calls that cannot be dispatched.
        "code_execution" | "code_interpreter" | "python" => unsupported(
            "no Vellum runtime backs Codex's hosted code interpreter; use the shell tool instead",
        ),
        // The schema of an MCP tool comes from the connected server. Inventing
        // one would be a guess, and a wrong guess on a mutating tool is unsafe.
        "mcp" => unsupported("MCP tool schemas are defined by the connected server and cannot be synthesised"),
        "computer" | "computer_use" => {
            unsupported("computer-use has no translated runtime on this route")
        }
        "image_generation" => unsupported("image generation is not available on translated routes"),
        other => BuiltinDecision::Unsupported {
            reason: format!("unknown Codex built-in `{other}` has no exact translated contract"),
        },
    }
}

/// Conservative default for anything not in the built-in table: assume a tool
/// can mutate unless it is known not to. Used to decide what may run in
/// parallel and what must never receive a permissive schema.
pub fn is_mutating_tool(name: &str) -> bool {
    const READ_ONLY: &[&str] = &[
        "web_search",
        "x_search",
        "file_search",
        "read_file",
        "search_files",
        "list_dir",
        "grep",
        "git_diff",
        "git_status",
        "tool_search",
        "view_image",
    ];
    !READ_ONLY.contains(&name)
}

/// The patch envelope, restated in full. Codex normally hands Sol a Lark
/// grammar for this; a translated provider cannot consume that grammar, so the
/// authority has to move into the description and be enforced by the parser in
/// [`validate_patch`].
pub fn apply_patch_description() -> String {
    [
        "Apply a patch to local files. This is the only supported way to edit files on disk: do not use shell redirection, sed, Set-Content, or temporary Python/PowerShell scripts to write files.",
        "`patch` must be one complete document:",
        "*** Begin Patch",
        "*** Update File: relative/path.ts",
        "@@ optional context header",
        " unchanged line (leading space)",
        "-removed line",
        "+added line",
        "*** Add File: relative/new.ts",
        "+every line of the new file, each prefixed with +",
        "*** Delete File: relative/old.ts",
        "*** End Patch",
        "Paths are relative to the workspace root and use forward slashes. Context lines keep their leading space. One call may contain several file sections. Prefer several small, exact patches over one broad rewrite.",
    ]
    .join("\n")
}

/// Exact translated contract. `additionalProperties: false` matters: with a
/// permissive object the model can invent a second field and silently lose the
/// patch body.
pub fn apply_patch_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "patch": {
                "type": "string",
                "description": "Complete patch document beginning with *** Begin Patch and ending with *** End Patch"
            }
        },
        "required": ["patch"],
        "additionalProperties": false
    })
}

pub fn apply_patch_function_tool() -> Value {
    json!({
        "type": "function",
        "name": APPLY_PATCH_TOOL_NAME,
        "description": apply_patch_description(),
        "parameters": apply_patch_parameters()
    })
}

/// The field a translated `apply_patch` call carries. The adapter maps
/// `function_call.patch` back onto the native `custom_tool_call.input` so the
/// real apply_patch runtime — permissions, diff tracking, patch progress
/// events — stays in the loop.
pub const APPLY_PATCH_FIELD: &str = "patch";

/// Pull the patch body out of a translated call, accepting the legacy `input`
/// field and a bare string so older histories keep replaying.
pub fn apply_patch_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| {
            value
                .get(APPLY_PATCH_FIELD)
                .or_else(|| value.get("input"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| arguments.to_string())
}

/// Validate a value against the subset of JSON Schema these contracts use:
/// `type`, `properties`, `required`, `additionalProperties`, `anyOf`, `items`.
///
/// This exists so "every tool call and history item this harness emits must
/// satisfy the schema it advertises" is a tested property rather than a claim.
/// A history item carrying a field its own schema forbids is the same
/// surface/runtime split as a spoofed capability flag, only smaller.
pub fn validate_against_schema(value: &Value, schema: &Value) -> Result<(), String> {
    validate_at(value, schema, "")
}

fn validate_at(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let at = |suffix: &str| {
        if path.is_empty() {
            suffix.to_string()
        } else {
            format!("{path}.{suffix}")
        }
    };
    let location = if path.is_empty() { "value" } else { path };

    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        return options
            .iter()
            .any(|option| validate_at(value, option, path).is_ok())
            .then_some(())
            .ok_or_else(|| format!("{location} matches none of the permitted shapes"));
    }

    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        if !matches {
            return Err(format!("{location} must be {expected}"));
        }
    }

    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                validate_at(item, item_schema, &at(&format!("[{index}]")))?;
            }
        }
    }

    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !object.contains_key(name) {
            return Err(format!("{location} is missing required field `{name}`"));
        }
    }
    for (name, field) in object {
        match properties.and_then(|properties| properties.get(name)) {
            Some(field_schema) => validate_at(field, field_schema, &at(name))?,
            None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                return Err(format!(
                    "{location} carries `{name}`, which the schema does not allow"
                ));
            }
            None => {}
        }
    }
    Ok(())
}

/// Fold namespace-level guidance into each child description.
///
/// Flattening alone throws away the grouping information the native surface
/// carried, and with it the model's sense of which tools belong together.
pub fn namespaced_description(
    namespace: &str,
    namespace_description: Option<&str>,
    child_description: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    parts.push(format!("[{namespace}]"));
    if let Some(guidance) = namespace_description
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        parts.push(guidance.to_string());
    }
    if let Some(description) = child_description
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        parts.push(description.to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutating_builtins_never_get_a_permissive_schema() {
        for kind in ["shell", "local_shell", "shell_command"] {
            let BuiltinDecision::Exact {
                parameters,
                mutating,
                ..
            } = builtin_tool(kind)
            else {
                panic!("{kind} must have an exact contract");
            };
            assert!(mutating);
            assert_eq!(parameters["additionalProperties"], false);
            // Both argv and single-script forms are accepted, because both
            // genuinely appear in history; neither is a permissive object.
            let command = &parameters["properties"]["command"];
            assert_eq!(command["anyOf"][0]["type"], "array");
            assert_eq!(command["anyOf"][0]["items"]["type"], "string");
            assert_eq!(command["anyOf"][1]["type"], "string");
            assert_eq!(parameters["required"], json!(["command"]));
        }
    }

    #[test]
    fn undispatchable_builtins_are_explicitly_unsupported() {
        for kind in ["code_execution", "code_interpreter", "mcp", "computer"] {
            let decision = builtin_tool(kind);
            assert!(!decision.is_supported(), "{kind} must not be exposed");
            assert!(decision.reason().is_some_and(|reason| !reason.is_empty()));
        }
        assert!(builtin_tool("brand_new_tool")
            .reason()
            .is_some_and(|reason| reason.contains("brand_new_tool")));
    }

    #[test]
    fn read_only_search_tools_keep_exact_schemas() {
        let BuiltinDecision::Exact {
            parameters,
            mutating,
            ..
        } = builtin_tool("web_search")
        else {
            panic!("web_search must stay available");
        };
        assert!(!mutating);
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(parameters["required"], json!(["query"]));
    }

    #[test]
    fn apply_patch_contract_is_exact_and_closed() {
        let tool = apply_patch_function_tool();
        assert_eq!(tool["name"], APPLY_PATCH_TOOL_NAME);
        assert_eq!(tool["parameters"]["additionalProperties"], false);
        assert_eq!(tool["parameters"]["required"], json!(["patch"]));
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("*** Begin Patch"));
        assert!(description.contains("*** End Patch"));
        // The prompt tells the model not to shell-edit; the tool it will
        // actually reach for has to repeat it.
        assert!(description.contains("temporary Python"));
    }

    #[test]
    fn apply_patch_input_accepts_the_exact_field_and_legacy_shapes() {
        assert_eq!(
            apply_patch_input(r#"{"patch":"*** Begin Patch\n*** End Patch"}"#),
            "*** Begin Patch\n*** End Patch"
        );
        assert_eq!(apply_patch_input(r#"{"input":"legacy"}"#), "legacy");
        assert_eq!(apply_patch_input("raw text"), "raw text");
        assert_eq!(apply_patch_input("   "), "");
    }

    #[test]
    fn normalizes_only_grok_style_patch_envelope_fences() {
        let source = "*** Begin Patch ***\n*** Update File: solution.py\n@@\n-old\n+new\n*** End Patch ***\n";
        let normalized = normalize_patch_delimiters(source);
        assert_eq!(
            normalized,
            "*** Begin Patch\n*** Update File: solution.py\n@@\n-old\n+new\n*** End Patch\n"
        );
        assert!(validate_patch(&normalized).is_ok());

        let unrelated = "prefix\n*** Begin Patch ***\nbody";
        assert_eq!(normalize_patch_delimiters(unrelated), unrelated);
    }

    #[test]
    fn valid_patch_reports_structured_file_changes() {
        let patch = "*** Begin Patch\n*** Update File: src/a.ts\n@@ header\n unchanged\n-old\n+new\n*** Add File: src/b.ts\n+created\n*** Delete File: src/c.ts\n*** End Patch";
        let summary = validate_patch(patch).unwrap();
        assert_eq!(summary.changes.len(), 3);
        assert_eq!(summary.changes[0].path, "src/a.ts");
        assert_eq!(summary.changes[0].kind, FileChangeKind::Update);
        assert_eq!(summary.changes[0].added_lines, 1);
        assert_eq!(summary.changes[0].removed_lines, 1);
        assert_eq!(summary.changes[1].kind, FileChangeKind::Add);
        assert_eq!(summary.changes[2].kind, FileChangeKind::Delete);
        assert_eq!(summary.to_json()["changes"][2]["kind"], "delete");
    }

    #[test]
    fn malformed_patches_name_the_specific_problem() {
        let missing_begin = validate_patch("*** Update File: a.ts\n+x\n*** End Patch").unwrap_err();
        assert!(missing_begin.contains("*** Begin Patch"));

        let missing_end = validate_patch("*** Begin Patch\n*** Add File: a.ts\n+x").unwrap_err();
        assert!(missing_end.contains("*** End Patch"));

        let bad_section =
            validate_patch("*** Begin Patch\n*** Move File: a.ts\n*** End Patch").unwrap_err();
        assert!(bad_section.contains("unknown patch section"));
        assert!(bad_section.contains("line 2"));

        let stray = validate_patch("*** Begin Patch\nhello\n*** End Patch").unwrap_err();
        assert!(stray.contains("content before any"));

        let empty_add =
            validate_patch("*** Begin Patch\n*** Add File: a.ts\n*** End Patch").unwrap_err();
        assert!(empty_add.contains("no `+` content lines"));

        let noop_update = validate_patch(
            "*** Begin Patch\n*** Update File: a.ts\n@@ header\n context\n*** End Patch",
        )
        .unwrap_err();
        assert!(noop_update.contains("would not change anything"));

        let no_sections = validate_patch("*** Begin Patch\n*** End Patch").unwrap_err();
        assert!(no_sections.contains("no file sections"));

        assert!(validate_patch("   ").unwrap_err().contains("empty"));
    }

    #[test]
    fn namespace_guidance_is_folded_into_child_descriptions() {
        let description = namespaced_description(
            "browser",
            Some("Drive the embedded browser."),
            Some("Read the page."),
        );
        assert_eq!(
            description,
            "[browser] Drive the embedded browser. Read the page."
        );
        assert_eq!(namespaced_description("x", None, None), "[x]");
        assert_eq!(flatten_namespace_name("x", "read"), "x__read");
    }

    #[test]
    fn unknown_tools_are_treated_as_mutating() {
        assert!(is_mutating_tool("apply_patch"));
        assert!(is_mutating_tool("some_new_tool"));
        assert!(!is_mutating_tool("read_file"));
        assert!(!is_mutating_tool("web_search"));
    }
}
