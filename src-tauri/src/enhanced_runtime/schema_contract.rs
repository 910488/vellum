//! Directional JSON Schema inclusion over the bridge's actual dependencies.
//! Unsupported constraints are indeterminate, never evidence of compatibility.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub type Documents = BTreeMap<String, Value>;
#[derive(Clone, Copy)]
struct Node<'a> {
    file: &'a str,
    value: &'a Value,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Inclusion {
    Compatible,
    Breaking(String),
    Unknown(String),
}

fn resolve<'a>(docs: &'a Documents, node: Node<'a>) -> Option<Node<'a>> {
    let Some(reference) = node.value.get("$ref").and_then(Value::as_str) else {
        return Some(node);
    };
    let (file, pointer) = reference.split_once('#').unwrap_or((reference, ""));
    let file = if file.is_empty() {
        node.file.to_string()
    } else {
        let parent = node
            .file
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let joined = if parent.is_empty() {
            file.into()
        } else {
            format!("{parent}/{file}")
        };
        let mut parts = Vec::new();
        for part in joined.split('/') {
            match part {
                ".." => {
                    parts.pop()?;
                }
                "." => {}
                _ => parts.push(part),
            }
        }
        parts.join("/")
    };
    let (file, doc) = docs.get_key_value(&file)?;
    Some(Node {
        file,
        value: if pointer.is_empty() {
            doc
        } else {
            doc.pointer(pointer)?
        },
    })
}

fn types(value: &Value) -> BTreeSet<String> {
    match &value["type"] {
        Value::String(value) => [value.clone()].into_iter().collect(),
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => BTreeSet::new(),
    }
}
fn required(value: &Value) -> BTreeSet<&str> {
    value["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
fn variants(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("anyOf")
        .or_else(|| value.get("oneOf"))
        .and_then(Value::as_array)
}
fn accepts(
    sender_docs: &Documents,
    reader_docs: &Documents,
    sender: Node<'_>,
    reader: Node<'_>,
    visited: &mut BTreeSet<(String, String)>,
    depth: usize,
) -> Inclusion {
    if depth > 64 {
        return Inclusion::Unknown("schema recursion limit".into());
    }
    let (Some(sender), Some(reader)) = (resolve(sender_docs, sender), resolve(reader_docs, reader))
    else {
        return Inclusion::Unknown("unresolved schema reference".into());
    };
    let key = (
        format!("{}:{}", sender.file, sender.value),
        format!("{}:{}", reader.file, reader.value),
    );
    if !visited.insert(key) {
        return if sender.value.get("$ref").is_some() || reader.value.get("$ref").is_some() {
            Inclusion::Unknown("cyclic reference without a value schema".into())
        } else {
            Inclusion::Compatible
        };
    }
    if sender.value.get("$ref").is_some() || reader.value.get("$ref").is_some() {
        return accepts(sender_docs, reader_docs, sender, reader, visited, depth + 1);
    }
    if reader.value == &Value::Bool(true) || sender.value == &Value::Bool(false) {
        return Inclusion::Compatible;
    }
    if reader.value == &Value::Bool(false) {
        return Inclusion::Breaking("reader rejects value".into());
    }
    for node in [sender, reader] {
        if variants(node.value).is_some()
            && node.value.as_object().is_some_and(|map| {
                map.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "oneOf"
                            | "anyOf"
                            | "$schema"
                            | "$id"
                            | "title"
                            | "description"
                            | "definitions"
                            | "$defs"
                            | "default"
                            | "examples"
                    )
                })
            })
        {
            return Inclusion::Unknown("union has additional constraints".into());
        }
    }
    if let Some(items) = variants(sender.value) {
        for value in items {
            let result = accepts(
                sender_docs,
                reader_docs,
                Node { value, ..sender },
                reader,
                &mut visited.clone(),
                depth + 1,
            );
            if result != Inclusion::Compatible {
                return result;
            }
        }
        return Inclusion::Compatible;
    }
    if let Some(items) = variants(reader.value) {
        let results: Vec<_> = items
            .iter()
            .map(|value| {
                accepts(
                    sender_docs,
                    reader_docs,
                    sender,
                    Node { value, ..reader },
                    &mut visited.clone(),
                    depth + 1,
                )
            })
            .collect();
        if results.contains(&Inclusion::Compatible) {
            return Inclusion::Compatible;
        }
        return if results.iter().any(|r| matches!(r, Inclusion::Unknown(_))) {
            Inclusion::Unknown("union acceptance is indeterminate".into())
        } else {
            Inclusion::Breaking("no reader union variant accepts sender".into())
        };
    }
    let sent = types(sender.value);
    let mut read = types(reader.value);
    if read.contains("number") {
        read.insert("integer".into());
    }
    if !read.is_empty() {
        if sent.is_empty() {
            return Inclusion::Unknown("sender type is unconstrained".into());
        }
        if !sent.is_subset(&read) {
            return Inclusion::Breaking("value type changed".into());
        }
    }
    let allowed = |v: &Value| -> Option<Vec<Value>> {
        v.get("const")
            .map(|value| vec![value.clone()])
            .or_else(|| v.get("enum").and_then(Value::as_array).cloned())
    };
    if let Some(reader_values) = allowed(reader.value) {
        let Some(sender_values) = allowed(sender.value) else {
            return Inclusion::Breaking("reader enum is narrower".into());
        };
        if sender_values.iter().any(|v| !reader_values.contains(v)) {
            return Inclusion::Breaking("enum value is not accepted".into());
        }
    }
    if !required(reader.value).is_subset(&required(sender.value)) {
        return Inclusion::Breaking("reader requires a field sender may omit".into());
    }
    if reader.value["additionalProperties"] == false
        && sender.value["additionalProperties"] != false
    {
        return Inclusion::Breaking("sender permits properties rejected by reader".into());
    }
    // A sender that lists its properties and says nothing about any others is a
    // plain struct: it serializes exactly the fields it lists. Codex's generator
    // marks the genuinely open objects — flattened maps, free-form bags — with
    // `additionalProperties: true` or a value schema, so only those can put
    // something unlisted on the wire. A reader property such a struct does not
    // list is one it never writes; whether the reader can live without it was
    // settled by the `required` check above.
    let sender_lists_every_field = sender.value["properties"].is_object()
        && sender.value.get("additionalProperties").is_none()
        && sender.value.get("patternProperties").is_none();
    if let Some(properties) = reader.value["properties"].as_object() {
        for (name, constraint) in properties {
            if sender.value["properties"].get(name).is_none()
                && !sender_lists_every_field
                && sender.value["additionalProperties"] != false
                && constraint != &Value::Bool(true)
                && constraint != &serde_json::json!({})
            {
                return Inclusion::Unknown(format!(
                    "sender does not constrain optional property {name}"
                ));
            }
        }
    }
    if let Some(properties) = sender.value["properties"].as_object() {
        for (name, value) in properties {
            if let Some(reader_value) = reader.value["properties"].get(name) {
                let result = accepts(
                    sender_docs,
                    reader_docs,
                    Node { value, ..sender },
                    Node {
                        value: reader_value,
                        ..reader
                    },
                    &mut visited.clone(),
                    depth + 1,
                );
                match result {
                    Inclusion::Compatible => {}
                    Inclusion::Breaking(reason) => {
                        return Inclusion::Breaking(format!("{name}: {reason}"))
                    }
                    Inclusion::Unknown(reason) => {
                        return Inclusion::Unknown(format!("{name}: {reason}"))
                    }
                }
            } else if reader.value["additionalProperties"] == false {
                return Inclusion::Breaking(format!("reader rejects property {name}"));
            } else if reader.value["additionalProperties"].is_object() {
                let result = accepts(
                    sender_docs,
                    reader_docs,
                    Node { value, ..sender },
                    Node {
                        value: &reader.value["additionalProperties"],
                        ..reader
                    },
                    &mut visited.clone(),
                    depth + 1,
                );
                if result != Inclusion::Compatible {
                    return result;
                }
            }
        }
    }
    if let Some(items) = reader.value.get("items") {
        let Some(sent_items) = sender.value.get("items") else {
            return Inclusion::Unknown("sender array items are unconstrained".into());
        };
        let result = accepts(
            sender_docs,
            reader_docs,
            Node {
                value: sent_items,
                ..sender
            },
            Node {
                value: items,
                ..reader
            },
            &mut visited.clone(),
            depth + 1,
        );
        if result != Inclusion::Compatible {
            return result;
        }
    }
    // Do not claim general JSON Schema theorem proving. Known-but-unhandled
    // constraints that change require qualification or a comparator extension.
    for keyword in [
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "pattern",
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "uniqueItems",
        "contains",
        "prefixItems",
        "dependentRequired",
        "patternProperties",
        "unevaluatedProperties",
    ] {
        if sender.value.get(keyword) != reader.value.get(keyword)
            && reader.value.get(keyword).is_some()
        {
            return Inclusion::Unknown(format!("changed constraint {keyword}"));
        }
    }
    Inclusion::Compatible
}

fn named<'a>(documents: &'a Documents, name: &str) -> Option<Node<'a>> {
    for (file, doc) in documents {
        if file.rsplit('/').next() == Some(&format!("{name}.json")) || doc["title"] == name {
            return Some(Node { file, value: doc });
        }
    }
    None
}
pub fn compare_named(sender: &Documents, reader: &Documents, name: &str) -> Inclusion {
    let (Some(sent), Some(read)) = (named(sender, name), named(reader, name)) else {
        return Inclusion::Unknown(format!("missing schema {name}"));
    };
    accepts(sender, reader, sent, read, &mut BTreeSet::new(), 0)
}

fn method_node<'a>(
    documents: &'a Documents,
    node: Node<'a>,
    method: &str,
    depth: usize,
) -> Option<Node<'a>> {
    if depth > 64 {
        return None;
    }
    let node = resolve(documents, node)?;
    if node.value.get("$ref").is_some() {
        return method_node(documents, node, method, depth + 1);
    }
    let tag = &node.value["properties"]["method"];
    if tag["const"] == method
        || tag["enum"]
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v == method))
    {
        return Some(node);
    }
    variants(node.value)?
        .iter()
        .find_map(|value| method_node(documents, Node { value, ..node }, method, depth + 1))
}

pub fn compare_method(
    sender: &Documents,
    reader: &Documents,
    envelope: &str,
    method: &str,
) -> Inclusion {
    let sent = named(sender, envelope).and_then(|node| method_node(sender, node, method, 0));
    let read = named(reader, envelope).and_then(|node| method_node(reader, node, method, 0));
    let (Some(sent), Some(read)) = (sent, read) else {
        return Inclusion::Unknown(format!("unresolved {envelope} contract for {method}"));
    };
    accepts(sender, reader, sent, read, &mut BTreeSet::new(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn docs(value: Value) -> Documents {
        [("Test.json".into(), value)].into_iter().collect()
    }
    #[test]
    fn optional_additions_and_union_reordering_are_compatible() {
        let a = docs(json!({"oneOf":[{"type":"string"},{"type":"null"}]}));
        let b = docs(json!({"oneOf":[{"type":"null"},{"type":"string"}]}));
        assert_eq!(compare_named(&a, &b, "Test"), Inclusion::Compatible);
        let a = docs(
            json!({"type":"object","additionalProperties":false,"properties":{"x":{"type":"string"}}}),
        );
        let b = docs(
            json!({"type":"object","properties":{"x":{"type":"string"},"y":{"type":"number"}}}),
        );
        assert_eq!(compare_named(&a, &b, "Test"), Inclusion::Compatible);
    }
    #[test]
    fn nested_reference_changes_are_checked() {
        let a = docs(
            json!({"type":"object","properties":{"x":{"$ref":"#/definitions/X"}},"definitions":{"X":{"type":"string"}}}),
        );
        let b = docs(
            json!({"type":"object","properties":{"x":{"$ref":"#/definitions/X"}},"definitions":{"X":{"type":"number"}}}),
        );
        assert!(matches!(
            compare_named(&a, &b, "Test"),
            Inclusion::Breaking(_)
        ));
    }
    #[test]
    fn missing_refs_are_not_compatible() {
        let a = docs(json!({"$ref":"missing.json"}));
        assert!(matches!(
            compare_named(&a, &a, "Test"),
            Inclusion::Unknown(_)
        ));
    }
    /// Codex Desktop 0.154 gave `Thread` an optional `originator` the 0.153
    /// core does not have. The core cannot send what its struct does not list,
    /// so the newer reader just never sees it — that alone kept Enhanced from
    /// arming against every thread method the bridge routes.
    #[test]
    fn a_plain_struct_never_sends_a_property_it_does_not_list() {
        let sender =
            docs(json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}));
        let reader = docs(json!({"type":"object","required":["id"],"properties":{
            "id":{"type":"string"},"originator":{"type":["string","null"]}}}));
        assert_eq!(
            compare_named(&sender, &reader, "Test"),
            Inclusion::Compatible
        );
        let reader = docs(
            json!({"type":"object","required":["id","originator"],"properties":{
            "id":{"type":"string"},"originator":{"type":"string"}}}),
        );
        assert!(matches!(
            compare_named(&sender, &reader, "Test"),
            Inclusion::Breaking(_)
        ));
    }
    #[test]
    fn an_open_sender_cannot_vouch_for_a_property_it_does_not_list() {
        let reader =
            docs(json!({"type":"object","properties":{"originator":{"type":["string","null"]}}}));
        for open in [json!(true), json!({"type":"number"})] {
            let sender = docs(json!({"type":"object","properties":{},"additionalProperties":open}));
            assert!(matches!(
                compare_named(&sender, &reader, "Test"),
                Inclusion::Unknown(_)
            ));
        }
        let sender = docs(json!({"type":"object"}));
        assert!(matches!(
            compare_named(&sender, &reader, "Test"),
            Inclusion::Unknown(_)
        ));
    }
}
