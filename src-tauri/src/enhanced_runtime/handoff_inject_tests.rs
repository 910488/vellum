use super::*;
use crate::enhanced_runtime::ThreadRuntimeBinding;

fn fixture() -> (tempfile::TempDir, ThreadRuntimeBindingStore, Inbox) {
    let temp = tempfile::tempdir().unwrap();
    let bindings = ThreadRuntimeBindingStore::open(temp.path().join("bindings.db")).unwrap();
    bindings
        .insert_immutable(&ThreadRuntimeBinding::new(
            "local",
            ExecutionPlane::EnhancedCodex,
            "digest",
            "provider",
            "model",
            "local",
            1,
        ))
        .unwrap();
    journal(temp.path(), "handoff", "completed");
    let inbox = Inbox::new(temp.path().into(), "launch".into());
    (temp, bindings, inbox)
}
fn journal(root: &Path, id: &str, state: &str) {
    let path = root.join("remote-handoffs/records");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join(format!("{id}.json")), serde_json::to_vec(&json!({"id":id,"source":{"threadId":"local"},
        "remote":{"state":state,"summary":"Implementation complete"},"result":{"outcome":null,"fetched":true,
        "capture":{"partial":false,"diffstat":[{"path":"file.rs","added":3,"deleted":1}]}},"patch":"MUST NOT SEND PATCH"})).unwrap()).unwrap();
}
fn own(inbox: &mut Inbox, bindings: &ThreadRuntimeBindingStore, client: u64, idle: Option<bool>) {
    let request = json!({"id":format!("resume-{client}"),"method":"thread/resume","params":{"threadId":"local"}});
    inbox.forwarded(client, ExecutionPlane::EnhancedCodex, &request);
    let status = idle.map(|idle| json!({"type":if idle { "idle" } else { "active" }}));
    inbox.observe(
        client,
        ExecutionPlane::EnhancedCodex,
        &json!({"id":request["id"],"result":{"thread":{"id":"local","status":status}}}),
        bindings,
    );
}
fn scan(inbox: &mut Inbox, bindings: &ThreadRuntimeBindingStore) -> Vec<Value> {
    let mut sent = Vec::new();
    inbox.scan(bindings, "digest", |client, value| {
        assert_eq!(client, 0);
        sent.push(value);
        true
    });
    sent
}
fn notify(inbox: &mut Inbox, bindings: &ThreadRuntimeBindingStore, method: &str, turn: &str) {
    inbox.observe(
        0,
        ExecutionPlane::EnhancedCodex,
        &json!({"method":method,"params":{"threadId":"local","turn":{"id":turn}}}),
        bindings,
    );
}

#[test]
fn publish_is_atomic_minimal_and_does_not_refresh_creation_time_or_repeat_terminal_requests() {
    let (temp, _, _) = fixture();
    publish(temp.path(), "handoff").unwrap();
    let path = directory(temp.path()).join("handoff.request");
    let bytes = fs::read(&path).unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(value["handoffId"], "handoff");
    publish(temp.path(), "handoff").unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::write(directory(temp.path()).join("handoff.sent"), b"intent").unwrap();
    fs::remove_file(&path).unwrap();
    publish(temp.path(), "handoff").unwrap();
    assert!(!path.exists());
    assert!(publish(temp.path(), "../escape").is_err());
}

#[test]
fn active_and_unknown_turns_defer_until_completion_and_payload_is_reference_only() {
    let (temp, bindings, mut inbox) = fixture();
    publish(temp.path(), "handoff").unwrap();
    assert!(scan(&mut inbox, &bindings).is_empty());
    own(&mut inbox, &bindings, 0, None);
    assert!(scan(&mut inbox, &bindings).is_empty());
    notify(&mut inbox, &bindings, "turn/started", "turn");
    assert!(scan(&mut inbox, &bindings).is_empty());
    assert!(!directory(temp.path()).join("handoff.sent").exists());
    notify(&mut inbox, &bindings, "turn/completed", "turn");
    let sent = scan(&mut inbox, &bindings);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["method"], "thread/inject_items");
    assert_eq!(sent[0]["params"]["threadId"], "local");
    let item = &sent[0]["params"]["items"][0];
    assert_eq!(item["role"], "user");
    assert_eq!(item["content"][0]["type"], "input_text");
    let content = item["content"][0]["text"].as_str().unwrap();
    assert!(content.starts_with(PREFIX));
    assert!(content.contains("結果尚未套用"));
    assert!(content.contains("file.rs\t+3\t-1"));
    assert!(content.contains("Implementation complete"));
    assert!(!content.contains("MUST NOT SEND PATCH"));
    assert!(directory(temp.path()).join("handoff.sent").exists());
}

#[test]
fn multiple_active_turns_and_pending_turn_start_block_injection_without_reactivating_a_finished_turn(
) {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    let request = json!({"id":"start","method":"turn/start","params":{"threadId":"local"}});
    inbox.forwarded(0, ExecutionPlane::EnhancedCodex, &request);
    assert!(scan(&mut inbox, &bindings).is_empty());
    notify(&mut inbox, &bindings, "turn/started", "one");
    notify(&mut inbox, &bindings, "turn/started", "two");
    notify(&mut inbox, &bindings, "turn/completed", "one");
    assert!(scan(&mut inbox, &bindings).is_empty());
    notify(&mut inbox, &bindings, "turn/completed", "two");
    assert!(scan(&mut inbox, &bindings).is_empty()); // start response is still outstanding
    inbox.observe(
        0,
        ExecutionPlane::EnhancedCodex,
        &json!({"id":"start","result":{"turn":{"id":"one","status":"inProgress"}}}),
        &bindings,
    );
    assert_eq!(scan(&mut inbox, &bindings).len(), 1);
}

#[test]
fn failed_turn_start_does_not_leave_an_idle_owner_blocked() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    inbox.forwarded(
        0,
        ExecutionPlane::EnhancedCodex,
        &json!({"id":7,"method":"turn/start","params":{"threadId":"local"}}),
    );
    assert!(scan(&mut inbox, &bindings).is_empty());
    inbox.observe(
        0,
        ExecutionPlane::EnhancedCodex,
        &json!({"id":7,"error":{"code":-32600}}),
        &bindings,
    );
    assert_eq!(scan(&mut inbox, &bindings).len(), 1);
}

#[test]
fn only_unique_live_enhanced_owner_can_inject_and_disconnect_releases_ambiguity() {
    let (temp, bindings, mut inbox) = fixture();
    publish(temp.path(), "handoff").unwrap();
    own(&mut inbox, &bindings, 0, Some(true));
    own(&mut inbox, &bindings, 2, Some(true));
    assert!(scan(&mut inbox, &bindings).is_empty());
    inbox.disconnected(2, None);
    assert_eq!(scan(&mut inbox, &bindings).len(), 1);
}

#[test]
fn intent_precedes_send_and_acknowledgement_is_idempotent_across_restart() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    let mut sent = Vec::new();
    inbox.scan(&bindings, "digest", |_, value| {
        assert!(directory(temp.path()).join("handoff.sent").exists());
        sent.push(value);
        true
    });
    let reply = json!({"id":sent[0]["id"],"result":{}});
    assert!(inbox.response(0, ExecutionPlane::EnhancedCodex, &reply));
    assert!(directory(temp.path()).join("handoff.delivered").exists());
    assert!(!directory(temp.path()).join("handoff.request").exists());
    drop(inbox);
    let mut next = Inbox::new(temp.path().into(), "next-launch".into());
    own(&mut next, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    assert!(scan(&mut next, &bindings).is_empty());
}

#[test]
fn wrong_socket_cannot_acknowledge_timeout_is_uncertain_and_late_replies_are_swallowed() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    let sent = scan(&mut inbox, &bindings);
    let reply = json!({"id":sent[0]["id"],"result":{}});
    assert!(inbox.response(2, ExecutionPlane::EnhancedCodex, &reply));
    assert!(inbox.response(0, ExecutionPlane::OfficialCodex, &reply));
    assert!(!directory(temp.path()).join("handoff.delivered").exists());
    inbox.flights.values_mut().next().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    assert!(scan(&mut inbox, &bindings).is_empty());
    assert!(directory(temp.path()).join("handoff.uncertain").exists());
    assert!(inbox.response(0, ExecutionPlane::EnhancedCodex, &reply));
    assert!(!directory(temp.path()).join("handoff.delivered").exists());
}

#[test]
fn old_unsent_request_survives_restart_but_prior_sent_intent_never_replays() {
    let (temp, bindings, mut inbox) = fixture();
    publish(temp.path(), "handoff").unwrap();
    assert!(scan(&mut inbox, &bindings).is_empty());
    drop(inbox);
    let mut restarted = Inbox::new(temp.path().into(), "restart".into());
    own(&mut restarted, &bindings, 0, Some(true));
    assert_eq!(scan(&mut restarted, &bindings).len(), 1);
    // Simulate recovery of an intent left by a crashed process, with no live lock.
    journal(temp.path(), "orphan", "completed");
    publish(temp.path(), "orphan").unwrap();
    fs::write(directory(temp.path()).join("orphan.sent"), b"{}").unwrap();
    assert!(scan(&mut restarted, &bindings).is_empty());
    assert!(directory(temp.path()).join("orphan.uncertain").exists());
}

#[test]
fn locks_prevent_another_bridge_from_settling_an_inflight_intent() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    let mut second = Inbox::new(temp.path().into(), "other".into());
    own(&mut second, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    let sent = scan(&mut inbox, &bindings);
    assert!(scan(&mut second, &bindings).is_empty());
    assert!(!directory(temp.path()).join("handoff.uncertain").exists());
    inbox.response(
        0,
        ExecutionPlane::EnhancedCodex,
        &json!({"id":sent[0]["id"],"result":{}}),
    );
    assert!(scan(&mut second, &bindings).is_empty());
}

#[test]
fn expired_invalid_official_and_incomplete_requests_are_skipped_without_send() {
    let (temp, bindings, mut inbox) = fixture();
    publish(temp.path(), "handoff").unwrap();
    let dir = directory(temp.path());
    fs::write(
        dir.join("handoff.request"),
        serde_json::to_vec(&Request {
            handoff_id: "handoff".into(),
            created_at: chrono::Utc::now().timestamp() - EXPIRES - 1,
        })
        .unwrap(),
    )
    .unwrap();
    assert!(scan(&mut inbox, &bindings).is_empty());
    assert!(fs::read_to_string(dir.join("handoff.skipped"))
        .unwrap()
        .contains("expired"));
    for (id, state) in [
        ("incomplete", "running"),
        ("invalid", "completed"),
        ("official", "completed"),
    ] {
        journal(temp.path(), id, state);
        publish(temp.path(), id).unwrap();
    }
    fs::write(
        dir.join("invalid.request"),
        br#"{"handoffId":"invalid","createdAt":1,"method":"turn/start"}"#,
    )
    .unwrap();
    let other = ThreadRuntimeBindingStore::open(temp.path().join("official.db")).unwrap();
    other
        .insert_immutable(&ThreadRuntimeBinding::new(
            "local",
            ExecutionPlane::OfficialCodex,
            "official",
            "openai",
            "model",
            "local",
            1,
        ))
        .unwrap();
    assert!(scan(&mut inbox, &other).is_empty());
    for id in ["incomplete", "invalid", "official"] {
        assert!(dir.join(format!("{id}.skipped")).exists());
        assert!(!dir.join(format!("{id}.sent")).exists());
    }
}

#[test]
fn runtime_mismatch_defers_and_rejection_or_send_failure_is_silent_and_terminal() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    publish(temp.path(), "handoff").unwrap();
    inbox.scan(&bindings, "wrong-digest", |_, _| panic!("wrong runtime"));
    let sent = scan(&mut inbox, &bindings);
    inbox.response(0, ExecutionPlane::EnhancedCodex, &json!({"id":sent[0]["id"],"error":{"code":-32600,"message":"already has an active writer"}}));
    assert!(directory(temp.path()).join("handoff.skipped").exists());
    journal(temp.path(), "nosocket", "completed");
    publish(temp.path(), "nosocket").unwrap();
    inbox.scan(&bindings, "digest", |_, _| false);
    assert!(directory(temp.path()).join("nosocket.skipped").exists());
    publish(temp.path(), "nosocket").unwrap();
    assert!(scan(&mut inbox, &bindings).is_empty());
}

#[test]
fn payload_is_bounded_utf8_and_status_remains_visible() {
    let (temp, _, _) = fixture();
    let path = temp.path().join("remote-handoffs/records/handoff.json");
    let mut value: Value = read_json(&path, 1_000_000).unwrap();
    value["remote"]["summary"] = Value::String("遠端摘要".repeat(4096));
    let mut record: Journal = serde_json::from_value(value).unwrap();
    record.result.outcome = Some("branched".into());
    let content = text(&record);
    assert!(content.len() <= MAX_TEXT);
    assert!(content.starts_with(PREFIX));
    assert!(content.contains("尚未套用至工作樹"));
    record.result.fetched = false;
    assert!(text(&record).contains("尚未取回檔案變更"));
    assert!(Inbox::from_attestation(
        &temp.path().join("enhanced-runtime/bridge-attestation.json"),
        "launch"
    )
    .is_some());
    assert!(Inbox::from_attestation(&temp.path().join("unrelated.json"), "launch").is_none());
}

#[test]
fn waiting_owners_cannot_starve_later_requests() {
    let (temp, bindings, mut inbox) = fixture();
    own(&mut inbox, &bindings, 0, Some(true));
    for n in 0..10 {
        let id = format!("a{n:02}");
        journal(temp.path(), &id, "completed");
        let path = temp
            .path()
            .join("remote-handoffs/records")
            .join(format!("{id}.json"));
        let mut record: Value = read_json(&path, 1_000_000).unwrap();
        record["source"]["threadId"] = json!("no-owner");
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        publish(temp.path(), &id).unwrap();
    }
    journal(temp.path(), "z", "completed");
    publish(temp.path(), "z").unwrap();
    assert!(scan(&mut inbox, &bindings).is_empty());
    assert_eq!(scan(&mut inbox, &bindings).len(), 1);
    assert!(directory(temp.path()).join("z.sent").exists());
}

#[test]
fn completed_journal_recovery_preserves_its_age() {
    let (temp, _, _) = fixture();
    let path = temp.path().join("remote-handoffs/records/handoff.json");
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs((EXPIRES + 60) as u64);
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    drop(file);
    recover(temp.path(), "handoff").unwrap();
    let request: Request =
        read_json(&directory(temp.path()).join("handoff.request"), 1024).unwrap();
    assert!(chrono::Utc::now().timestamp() - request.created_at >= EXPIRES);
}
