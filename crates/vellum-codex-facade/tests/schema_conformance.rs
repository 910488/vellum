//! Schema conformance against the pinned Codex App Server protocol (§27, §28).
//!
//! The facade must not invent protocol. Every method, notification, server
//! request, thread-item type and status string it names is checked against the
//! generated schema for the pinned Codex version. A name that Codex does not
//! actually have fails here rather than reaching a UI that will never send or
//! understand it.
//!
//! Regenerate the pin with:
//!   codex app-server generate-json-schema --out third_party/codex-app-server-schema/<version>

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;
use vellum_codex_facade::methods::{self, item, notify, request, tool_status, MethodClass};

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("workspace root")
        .join("third_party/codex-app-server-schema/0.142.5")
}

fn load(name: &str) -> Value {
    let path = schema_dir().join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "pinned schema {} is missing ({error}). Regenerate it with \
             `codex app-server generate-json-schema --out {}`",
            path.display(),
            schema_dir().display()
        )
    });
    serde_json::from_str(&raw).expect("pinned schema is valid JSON")
}

/// Collects every `method` const/enum declared anywhere in a schema document.
fn declared_methods(document: &Value) -> HashSet<String> {
    fn walk(node: &Value, found: &mut HashSet<String>) {
        match node {
            Value::Object(map) => {
                if let Some(Value::Object(properties)) = map.get("properties") {
                    if let Some(method) = properties.get("method") {
                        if let Some(Value::String(constant)) = method.get("const") {
                            found.insert(constant.clone());
                        }
                        if let Some(Value::Array(values)) = method.get("enum") {
                            found
                                .extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
                        }
                    }
                }
                for value in map.values() {
                    walk(value, found);
                }
            }
            Value::Array(values) => values.iter().for_each(|value| walk(value, found)),
            _ => {}
        }
    }
    let mut found = HashSet::new();
    walk(document, &mut found);
    found
}

/// Collects every string in any `enum` whose sibling `title` matches.
fn declared_enum_values(document: &Value, title: &str) -> HashSet<String> {
    fn walk(node: &Value, title: &str, found: &mut HashSet<String>) {
        match node {
            Value::Object(map) => {
                if map.get("title").and_then(Value::as_str) == Some(title) {
                    if let Some(Value::Array(values)) = map.get("enum") {
                        found.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
                    }
                }
                for value in map.values() {
                    walk(value, title, found);
                }
            }
            Value::Array(values) => values.iter().for_each(|value| walk(value, title, found)),
            _ => {}
        }
    }
    let mut found = HashSet::new();
    walk(document, title, &mut found);
    found
}

/// Reads `definitions.<name>.enum`, for enums declared as named definitions
/// rather than titled inline schemas.
fn definition_enum_values(document: &Value, name: &str) -> HashSet<String> {
    document
        .get("definitions")
        .and_then(|definitions| definitions.get(name))
        .and_then(|definition| definition.get("enum"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_method_the_facade_names_exists_in_the_pinned_schema() {
    let declared = declared_methods(&load("ClientRequest.json"));
    assert!(
        declared.len() > 50,
        "the pinned ClientRequest schema looks truncated: {} methods",
        declared.len()
    );
    for method in methods::KNOWN_METHODS {
        assert!(
            declared.contains(*method),
            "`{method}` is not a Codex client request in the pinned schema. \
             The facade must not invent protocol."
        );
    }
}

#[test]
fn methods_the_facade_does_not_serve_are_still_real_codex_methods() {
    // Guards the inverse mistake: refusing a method by a misspelled name means
    // the real one silently falls through to the catch-all instead.
    let declared = declared_methods(&load("ClientRequest.json"));
    for method in methods::KNOWN_METHODS {
        if methods::classify(method) == MethodClass::Unsupported {
            assert!(declared.contains(*method));
        }
    }
}

#[test]
fn every_notification_the_facade_emits_exists_in_the_pinned_schema() {
    let declared = declared_methods(&load("ServerNotification.json"));
    for notification in notify::ALL {
        assert!(
            declared.contains(*notification),
            "`{notification}` is not a Codex server notification in the pinned schema"
        );
    }
}

#[test]
fn every_server_request_the_facade_sends_exists_in_the_pinned_schema() {
    let declared = declared_methods(&load("ServerRequest.json"));
    for server_request in request::ALL {
        assert!(
            declared.contains(*server_request),
            "`{server_request}` is not a Codex server request in the pinned schema"
        );
    }
}

#[test]
fn thread_item_types_and_tool_statuses_match_the_pinned_schema() {
    let document = load("v2/ItemStartedNotification.json");
    for (constant, title) in [
        (item::AGENT_MESSAGE, "AgentMessageThreadItemType"),
        (item::REASONING, "ReasoningThreadItemType"),
        (item::DYNAMIC_TOOL_CALL, "DynamicToolCallThreadItemType"),
        (item::PLAN, "PlanThreadItemType"),
        (item::SUB_AGENT_ACTIVITY, "SubAgentActivityThreadItemType"),
        (item::CONTEXT_COMPACTION, "ContextCompactionThreadItemType"),
    ] {
        let declared = declared_enum_values(&document, title);
        assert!(
            declared.contains(constant),
            "thread item type `{constant}` is not {title} in the pinned schema (found {declared:?})"
        );
    }

    // Status enums are named definitions rather than titled inline schemas.
    let statuses = definition_enum_values(&document, "DynamicToolCallStatus");
    for status in [
        tool_status::IN_PROGRESS,
        tool_status::COMPLETED,
        tool_status::FAILED,
    ] {
        assert!(
            statuses.contains(status),
            "`{status}` is not a DynamicToolCallStatus in the pinned schema (found {statuses:?})"
        );
    }
}

#[test]
fn the_pin_records_the_version_it_was_generated_from() {
    let version = std::fs::read_to_string(schema_dir().join("VERSION")).expect("VERSION exists");
    assert!(
        version.contains("0.142.5"),
        "the pinned VERSION does not match the pinned directory: {version}"
    );
}
