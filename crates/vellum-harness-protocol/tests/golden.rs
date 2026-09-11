//! Golden wire-format tests for the harness-neutral contract.
//!
//! These fixtures are the compatibility surface between adapters, the facade,
//! the journal and the evaluation tooling. Changing one is a protocol change,
//! so the fixture has to change with it in the same commit.

use std::path::PathBuf;

use serde_json::{json, Value};
use vellum_harness_protocol::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/events")
}

fn load(name: &str) -> Value {
    let path = golden_dir().join(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing golden fixture {}: {error}", path.display()));
    serde_json::from_str(&raw).expect("golden fixture is valid JSON")
}

#[test]
fn every_golden_event_round_trips_byte_for_byte() {
    let mut checked = 0;
    for entry in std::fs::read_dir(golden_dir()).expect("golden directory exists") {
        let path = entry.expect("readable golden entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let envelope: HarnessEventEnvelope = serde_json::from_value(expected.clone())
            .unwrap_or_else(|error| panic!("{} does not decode: {error}", path.display()));
        let re_encoded = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            re_encoded,
            expected,
            "{} does not re-encode to itself",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 4, "expected the golden corpus to be populated");
}

#[test]
fn unknown_native_payloads_survive_a_full_round_trip() {
    let envelope: HarnessEventEnvelope = serde_json::from_value(load("native-extension.json"))
        .expect("native extension fixture decodes");
    let HarnessEvent::NativeExtension(extension) = &envelope.event else {
        panic!("fixture is not a native extension event");
    };
    assert_eq!(extension.namespace, "xai");
    assert_eq!(
        extension.payload,
        json!({"repeats": 3, "tool": "read_file"})
    );
}

#[test]
fn descriptors_round_trip_with_camel_case_field_names() {
    let descriptor = HarnessDescriptor {
        id: HarnessId(HarnessId::GROK_BUILD.into()),
        display_name: "Grok Build".into(),
        vendor: "xAI".into(),
        transport: HarnessTransportKind::AcpStdio,
        process_scope: ProcessScope::PerWorkspace,
        capabilities: HarnessCapabilities {
            session_resume: true,
            compaction: true,
            ..Default::default()
        },
    };
    let encoded = serde_json::to_value(&descriptor).unwrap();
    assert_eq!(encoded["id"], json!("grok-build"));
    assert_eq!(encoded["displayName"], json!("Grok Build"));
    assert_eq!(encoded["transport"], json!("acpStdio"));
    assert_eq!(encoded["processScope"], json!("perWorkspace"));
    assert_eq!(encoded["capabilities"]["sessionResume"], json!(true));
    assert_eq!(encoded["capabilities"]["plans"], json!(false));
    assert_eq!(
        serde_json::from_value::<HarnessDescriptor>(encoded).unwrap(),
        descriptor
    );
}

#[test]
fn capabilities_default_to_absent_so_a_new_field_is_never_silently_claimed() {
    let capabilities = HarnessCapabilities::default();
    let encoded = serde_json::to_value(&capabilities).unwrap();
    for (field, value) in encoded.as_object().unwrap() {
        assert_eq!(value, &json!(false), "capability {field} defaults to true");
    }
}

#[test]
fn harness_ids_are_stable_and_reject_empty_values() {
    assert_eq!(HarnessId::CODEX, "codex");
    assert_eq!(HarnessId::GROK_BUILD, "grok-build");
    assert_eq!(HarnessId::QWEN_CODE, "qwen-code");
    assert_eq!(HarnessId::DEEPSEEK_HARNESS, "deepseek-harness");
    assert_eq!(HarnessId::VELLUM_GENERIC, "vellum-generic");

    assert_eq!(HarnessId::new(""), Err(HarnessIdError::Empty));
    assert_eq!(HarnessId::new("   "), Err(HarnessIdError::Empty));
    assert_eq!(
        HarnessId::new("grok-build").unwrap(),
        HarnessId("grok-build".into())
    );
    // Transparent representation: an id is a bare string on the wire.
    assert_eq!(
        serde_json::to_value(HarnessId("grok-build".into())).unwrap(),
        json!("grok-build")
    );
}

#[test]
fn error_categories_round_trip_across_the_whole_taxonomy() {
    for category in [
        HarnessErrorCategory::RuntimeUnavailable,
        HarnessErrorCategory::RuntimeCrashed,
        HarnessErrorCategory::Transport,
        HarnessErrorCategory::Protocol,
        HarnessErrorCategory::Authentication,
        HarnessErrorCategory::Permission,
        HarnessErrorCategory::UnsupportedCapability,
        HarnessErrorCategory::SessionNotFound,
        HarnessErrorCategory::SessionConflict,
        HarnessErrorCategory::Provider,
        HarnessErrorCategory::ResourceBudget,
        HarnessErrorCategory::Internal,
    ] {
        let encoded = serde_json::to_value(category).unwrap();
        assert_eq!(
            serde_json::from_value::<HarnessErrorCategory>(encoded).unwrap(),
            category
        );
    }
}

#[test]
fn the_journal_is_declared_non_authoritative_in_the_protocol_itself() {
    assert!(!EVENT_JOURNAL_RUNTIME_REPLAY_SOURCE);
}
