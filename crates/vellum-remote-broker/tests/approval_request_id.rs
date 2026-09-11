use serde_json::json;
use vellum_remote_broker::app_server::jsonrpc::JsonRpcId;
use vellum_remote_broker::approval_registry::ApprovalRegistry;
use vellum_remote_broker::db::BrokerDatabase;

fn open_registry() -> ApprovalRegistry {
    let db = BrokerDatabase::open_in_memory().unwrap();
    ApprovalRegistry::from_shared(db.connection())
}

#[test]
fn preserves_numeric_request_id() {
    let registry = open_registry();
    let stored = registry
        .register(
            7,
            JsonRpcId::Number(61),
            "item/commandExecution/requestApproval",
            json!({"command": "ls"}),
            Some("thr_1".into()),
            None,
            None,
        )
        .unwrap();
    let loaded = registry
        .get(&stored.approval.approval_token)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.upstream_request_id, JsonRpcId::Number(61));
    assert_eq!(loaded.upstream_epoch, 7);
}

#[test]
fn preserves_string_request_id() {
    let registry = open_registry();
    let stored = registry
        .register(
            3,
            JsonRpcId::String("abc".into()),
            "item/fileChange/requestApproval",
            json!({"path": "src/main.rs"}),
            Some("thr_2".into()),
            None,
            None,
        )
        .unwrap();
    let loaded = registry
        .get(&stored.approval.approval_token)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.upstream_request_id, JsonRpcId::String("abc".into()));
}

#[test]
fn routes_approval_frame_to_registry_via_classifier() {
    use vellum_remote_broker::app_server::jsonrpc::{classify_incoming, IncomingMessage};

    let message = classify_incoming(json!({
        "id": 61,
        "method": "item/commandExecution/requestApproval",
        "params": {
            "threadId": "thr_123",
            "command": "ls"
        }
    }))
    .unwrap();
    let IncomingMessage::Request(request) = message else {
        panic!("approval frame must classify as server request");
    };

    let registry = open_registry();
    let stored = registry
        .register(
            1,
            request.id,
            request.method,
            request.params,
            Some("thr_123".into()),
            None,
            None,
        )
        .unwrap();
    assert_eq!(stored.upstream_request_id, JsonRpcId::Number(61));
}
