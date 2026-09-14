use super::*;

#[test]
fn desktop_feature_flags_map_to_the_session_loader_profile() {
    for profile in [
        AblationProfile::E0,
        AblationProfile::E1,
        AblationProfile::E2,
        AblationProfile::E3,
        AblationProfile::E4,
        AblationProfile::E5,
    ] {
        assert_eq!(
            ablation_profile_label(profile.features()),
            Some(profile.as_str()),
            "the bridge and the Enhanced session loader must select the same hooks"
        );
    }
    assert_eq!(
        ablation_profile_label(EnhancedRuntimeFeatures {
            qwen_tool_reliability: true,
            deepseek_context_recovery: false,
            qwen_bounded_continuation: true,
            repetition_notice: false,
            intent_continuation: false,
        }),
        None,
        "a custom JSON feature set must not be overwritten by an E0 compatibility selector"
    );
}
use crate::enhanced_runtime::qualification::{JournalEntry, ENHANCED_EVENT_NOTIFICATION};
use crate::enhanced_runtime::ModelProviderRoute;

fn state(temp: &tempfile::TempDir) -> BridgeState {
    let attestation = AttestationWriter::new(
        temp.path().join("attestation.json"),
        "launch-test".into(),
        "sha256:map".into(),
        temp.path().join("bindings.sqlite"),
        ChildAttestation {
            binary_sha256: "sha256:official".into(),
            runtime_digest: "official-digest".into(),
            ..ChildAttestation::default()
        },
        ChildAttestation {
            binary_sha256: "sha256:enhanced".into(),
            runtime_digest: "enhanced-digest".into(),
            ..ChildAttestation::default()
        },
    );
    BridgeState::new(
        ThreadRuntimeBindingStore::open(temp.path().join("bindings.sqlite")).unwrap(),
        TrustedProviderSet::new(
            vec!["openai".into()],
            vec!["qwen".into(), "deepseek".into(), "grok".into()],
        ),
        "official-digest".into(),
        "enhanced-digest".into(),
        TrustedModelProviderMap::default(),
        attestation,
        QualificationJournal::new(temp.path().join("journal.jsonl"), "launch-test".into()),
    )
}

fn start_enhanced_thread(state: &mut BridgeState, provider: &str, model: &str, thread: &str) {
    let request =
        json!({"id": 1, "method": "thread/start", "params": {"modelProvider": provider, "model": model}})
            .to_string();
    state.on_client_line(&request).unwrap();
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            &json!({"id": 1, "result": {"thread": {"id": thread}}}).to_string(),
        )
        .unwrap();
}

#[test]
fn start_routes_by_trusted_provider_and_binds_returned_thread() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let input = r#"{"id":1,"method":"thread/start","params":{"modelProvider":"qwen","model":"qwen3-coder"}}"#;
    let actions = state.on_client_line(input).unwrap();
    assert_eq!(
        actions[0],
        BridgeAction::ToChild(
            ExecutionPlane::EnhancedCodex,
            serde_json::from_str(input).unwrap()
        )
    );
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            r#"{"id":1,"result":{"thread":{"id":"thread-1"}}}"#,
        )
        .unwrap();
    let binding = state.store.get("thread-1").unwrap().unwrap();
    assert_eq!(binding.plane, ExecutionPlane::EnhancedCodex);
    assert_eq!(binding.runtime_digest, "enhanced-digest");
    assert_eq!(binding.provider_id, "qwen");
}

#[test]
fn resume_cancel_and_approval_stay_on_bound_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    state
        .on_client_line(
            r#"{"id":1,"method":"thread/start","params":{"modelProvider":"deepseek","model":"deepseek-v4"}}"#,
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            r#"{"id":1,"result":{"thread":{"id":"thread-1"}}}"#,
        )
        .unwrap();
    for method in ["thread/resume", "turn/interrupt", "command/exec/approve"] {
        let line =
            json!({"id": 2, "method": method, "params": {"threadId": "thread-1"}}).to_string();
        let actions = state.on_client_line(&line).unwrap();
        assert!(matches!(
            &actions[0],
            BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, _)
        ));
    }
}

#[test]
fn remote_control_management_is_owned_by_the_transport_not_an_execution_core() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    for method in [
        "remoteControl/enable",
        "remoteControl/status/read",
        "remoteControl/pairing/start",
        "remoteControl/client/list",
    ] {
        let line = json!({"id": 7, "method": method, "params": {}}).to_string();
        let actions = state.on_client_line(&line).unwrap();
        assert!(matches!(&actions[0], BridgeAction::ToRelay(_)));
    }
}

#[test]
fn a_build_without_the_relay_keeps_mobile_control_on_official() {
    // Desktop creates native conversations on Official. Enhanced can discover
    // their files through the shared CODEX_HOME, but it cannot read or resume
    // an Official process's active writer. The no-sidecar runtime therefore
    // has to keep the one available Remote Control owner on Official.
    assert_eq!(FALLBACK_REMOTE_CONTROL_PLANE, ExecutionPlane::OfficialCodex);
    assert!(owns_fallback_remote_control(ExecutionPlane::OfficialCodex));
    assert!(!owns_fallback_remote_control(ExecutionPlane::EnhancedCodex));

    let mut official = std::process::Command::new("official");
    configure_fallback_remote_control(&mut official, ExecutionPlane::OfficialCodex, true);
    assert!(official
        .get_envs()
        .all(|(name, _)| name != REMOTE_CONTROL_DISABLED_ENV));

    let mut enhanced = std::process::Command::new("enhanced");
    configure_fallback_remote_control(&mut enhanced, ExecutionPlane::EnhancedCodex, true);
    assert!(enhanced.get_envs().any(|(name, value)| {
        name == REMOTE_CONTROL_DISABLED_ENV && value == Some(std::ffi::OsStr::new("1"))
    }));

    // User-scoped CODEX_CLI_PATH also reaches short-lived helpers. They are
    // not the Desktop Remote Control owner and must never open a competing
    // connection with the same installation id.
    let mut transient_official = std::process::Command::new("official");
    configure_fallback_remote_control(
        &mut transient_official,
        ExecutionPlane::OfficialCodex,
        false,
    );
    assert!(transient_official.get_envs().any(|(name, value)| {
        name == REMOTE_CONTROL_DISABLED_ENV && value == Some(std::ffi::OsStr::new("1"))
    }));

    assert!(should_isolate_bridge_state(false, true));
    assert!(!should_isolate_bridge_state(true, true));
    assert!(!should_isolate_bridge_state(false, false));
}

/// The phone failure this branch exists for: `thread/resume` reached the
/// Enhanced core for a thread Official owns, and the Enhanced core answered
/// `thread-store conflict: … already has an active writer`. Routing is by the
/// immutable binding for every frontend, so an Official-bound thread resumes on
/// Official no matter which client asked.
#[test]
fn a_thread_bound_to_official_resumes_on_official_for_any_frontend() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    state
        .on_client_line(
            r#"{"id":1,"method":"thread/start","params":{"modelProvider":"openai","model":"gpt-5.1-codex"}}"#,
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            r#"{"id":1,"result":{"thread":{"id":"official-thread"}}}"#,
        )
        .unwrap();
    for method in ["thread/read", "thread/resume", "turn/start"] {
        let line = json!({"id": 2, "method": method, "params": {"threadId": "official-thread"}})
            .to_string();
        let actions = state.on_client_line(&line).unwrap();
        assert!(
            matches!(
                &actions[0],
                BridgeAction::ToChild(ExecutionPlane::OfficialCodex, _)
            ),
            "{method} must reach the core that already holds the writer"
        );
    }
}

/// Resuming a thread whose owner is unknown must say so. Trying each core in
/// turn is what creates the second writer.
#[test]
fn a_thread_operation_without_an_owner_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    for method in ["thread/read", "thread/resume", "turn/start", "turn/steer"] {
        let line = json!({"id": 3, "method": method, "params": {}}).to_string();
        assert!(
            state.on_client_line(&line).is_err(),
            "{method} must not default to a core when no thread owner is known"
        );
    }
}

#[test]
fn provider_switch_and_unknown_provider_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    state
        .on_client_line(
            r#"{"id":1,"method":"thread/start","params":{"modelProvider":"openai","model":"gpt-5"}}"#,
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            r#"{"id":1,"result":{"thread":{"id":"thread-1"}}}"#,
        )
        .unwrap();
    let switched = state.on_client_line(
        r#"{"id":2,"method":"thread/resume","params":{"threadId":"thread-1","modelProvider":"qwen"}}"#,
    );
    assert!(matches!(
        switched,
        Err(BridgeError::ProviderSwitchForbidden { .. })
    ));
    let unknown = state.on_client_line(
        r#"{"id":3,"method":"thread/start","params":{"modelProvider":"not-trusted","model":"x"}}"#,
    );
    assert!(matches!(unknown, Err(BridgeError::Router(_))));
}

#[test]
fn server_request_ids_are_namespaced_and_routed_back() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let actions = state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            r#"{"id":7,"method":"item/commandExecution/requestApproval","params":{}}"#,
        )
        .unwrap();
    let BridgeAction::ToClient(request) = &actions[0] else {
        panic!("expected client request")
    };
    let translated = request["id"].as_str().unwrap();
    let response = json!({"id": translated, "result": {"decision": "accept"}}).to_string();
    let actions = state.on_client_line(&response).unwrap();
    let BridgeAction::ToChild(plane, response) = &actions[0] else {
        panic!("expected child response")
    };
    assert_eq!(*plane, ExecutionPlane::EnhancedCodex);
    assert_eq!(response["id"], 7);
}

#[test]
fn initialize_reaches_both_children_and_only_ready_after_both_answer() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let actions = state
        .on_client_line(
            r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"desktop","title":"Desktop","version":"1"}}}"#,
        )
        .unwrap();
    assert_eq!(actions.len(), 2);
    assert!(matches!(
        &actions[0],
        BridgeAction::ToChild(ExecutionPlane::OfficialCodex, _)
    ));
    let BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, shadow) = &actions[1] else {
        panic!("expected enhanced initialize")
    };
    let shadow_id = shadow["id"].clone();
    assert_ne!(shadow_id, 1);

    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            r#"{"id":1,"result":{"userAgent":"codex"}}"#,
        )
        .unwrap();
    assert_eq!(
        state.attestation.snapshot().state,
        crate::enhanced_runtime::BridgeLifecycle::Starting
    );

    let response = json!({"id": shadow_id, "result": {}}).to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &response)
        .unwrap()
        .is_empty());
    assert_eq!(
        state.attestation.snapshot().state,
        crate::enhanced_runtime::BridgeLifecycle::Ready
    );

    let initialized = state.on_client_line(r#"{"method":"initialized"}"#).unwrap();
    assert_eq!(initialized.len(), 2);
    for action in initialized {
        let BridgeAction::ToChild(_, notification) = action else {
            panic!("expected initialized notification for each child")
        };
        assert_eq!(notification, json!({"method": "initialized"}));
    }
}

#[test]
fn trusted_catalog_map_splits_models_behind_the_shared_vellum_gateway() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    state.model_provider_map.models.insert(
        "gpt-catalog".into(),
        ModelProviderRoute {
            provider_id: "openai".into(),
            child_provider_id: "vellum-official".into(),
        },
    );
    state.model_provider_map.models.insert(
        "qwen-catalog".into(),
        ModelProviderRoute {
            provider_id: "qwen".into(),
            child_provider_id: "vellum".into(),
        },
    );

    let official = state
        .on_client_line(
            r#"{"id":1,"method":"thread/start","params":{"modelProvider":"vellum","model":"gpt-catalog"}}"#,
        )
        .unwrap();
    let BridgeAction::ToChild(plane, request) = &official[0] else {
        panic!("expected child request")
    };
    assert_eq!(*plane, ExecutionPlane::OfficialCodex);
    assert_eq!(request["params"]["modelProvider"], "vellum-official");

    let enhanced = state
        .on_client_line(
            r#"{"id":2,"method":"thread/start","params":{"modelProvider":"vellum","model":"qwen-catalog"}}"#,
        )
        .unwrap();
    let BridgeAction::ToChild(plane, request) = &enhanced[0] else {
        panic!("expected child request")
    };
    assert_eq!(*plane, ExecutionPlane::EnhancedCodex);
    assert_eq!(request["params"]["modelProvider"], "vellum");
}

#[test]
fn a_dead_enhanced_child_fails_third_party_turns_closed_and_leaves_official_alone() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "thread-1");
    state
        .on_client_line(
            r#"{"id":2,"method":"thread/start","params":{"modelProvider":"openai","model":"gpt-5"}}"#,
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            r#"{"id":2,"result":{"thread":{"id":"thread-official"}}}"#,
        )
        .unwrap();

    state.mark_child_exited(ExecutionPlane::EnhancedCodex, "app-server exited");

    for line in [
        r#"{"id":3,"method":"thread/resume","params":{"threadId":"thread-1"}}"#,
        r#"{"id":4,"method":"thread/start","params":{"modelProvider":"qwen","model":"qwen3-coder"}}"#,
        r#"{"id":5,"method":"turn/create","params":{"threadId":"thread-1"}}"#,
    ] {
        assert!(
            matches!(
                state.on_client_line(line),
                Err(BridgeError::RuntimeUnavailable {
                    plane: ExecutionPlane::EnhancedCodex
                })
            ),
            "{line} must fail closed instead of falling back to Official"
        );
    }

    let official = state
        .on_client_line(
            r#"{"id":6,"method":"turn/create","params":{"threadId":"thread-official"}}"#,
        )
        .unwrap();
    assert!(matches!(
        &official[0],
        BridgeAction::ToChild(ExecutionPlane::OfficialCodex, _)
    ));
    assert_eq!(
        state.attestation.snapshot().state,
        crate::enhanced_runtime::BridgeLifecycle::Degraded
    );
}

#[test]
fn enhanced_notifications_are_absorbed_and_never_reach_desktop() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let journal_path = temp.path().join("journal.jsonl");

    let identity = json!({
        "method": ENHANCED_IDENTITY_NOTIFICATION,
        "params": {
            "enhancedCommit": "c".repeat(40),
            "runtimeDigest": "sha256:enhanced",
            "featureProfile": "E5",
            "ports": {
                "qwenToolReliability": true,
                "deepseekContextRecovery": true,
                "qwenBoundedContinuation": true
            }
        }
    })
    .to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &identity)
        .unwrap()
        .is_empty());
    assert!(state.attestation.snapshot().enhanced_identity.is_some());

    let event = json!({
        "method": ENHANCED_EVENT_NOTIFICATION,
        "params": {
            "name": "enhanced.tool.duplicate_suppressed",
            "fields": {"callIdHash": "sha256:abc"}
        }
    })
    .to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &event)
        .unwrap()
        .is_empty());
    assert_eq!(
        state.journal.counts()["enhanced.tool.duplicate_suppressed"],
        1
    );

    let session_features = json!({
        "method": ENHANCED_EVENT_NOTIFICATION,
        "params": {
            "name": "enhanced.session.features_applied",
            "fields": {"featureProfile": "E5"}
        }
    })
    .to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &session_features)
        .unwrap()
        .is_empty());
    assert_eq!(state.attestation.snapshot().session_features_applied, 1);
    assert_eq!(
        state.journal.counts()["enhanced.session.features_applied"],
        1
    );

    let unknown = json!({"method": "vellum/somethingNew", "params": {}}).to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &unknown)
        .unwrap()
        .is_empty());

    let entries = QualificationJournal::read(&journal_path);
    assert!(entries
        .iter()
        .any(|entry| matches!(entry, JournalEntry::Identity { .. })));
    assert!(entries
        .iter()
        .any(|entry| matches!(entry, JournalEntry::Rejected { method, .. } if method == "vellum/somethingNew")));
}

#[test]
fn native_spawn_lifecycle_is_forwarded_verbatim_and_binds_its_child() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "parent-thread");
    let notification = json!({
        "method": "item/completed",
        "params": {
            "threadId": "parent-thread",
            "turnId": "parent-turn",
            "item": {
                "type": "collabAgentToolCall",
                "id": "spawn-call",
                "tool": "spawnAgent",
                "status": "completed",
                "senderThreadId": "parent-thread",
                "receiverThreadIds": ["child-thread"],
                "agentsStates": {"child-thread": {"status": "pendingInit"}},
                "prompt": "Inspect the real project",
                "model": "qwen3-coder",
                "reasoningEffort": "medium"
            },
            "completedAtMs": 42
        }
    });

    let actions = state
        .on_child_line(ExecutionPlane::EnhancedCodex, &notification.to_string())
        .unwrap();
    assert_eq!(actions, vec![BridgeAction::ToClient(notification)]);

    let child = state.store.get("child-thread").unwrap().unwrap();
    assert_eq!(child.plane, ExecutionPlane::EnhancedCodex);
    assert_eq!(child.provider_id, "qwen");
    assert_eq!(child.model_id, "qwen3-coder");
    assert_eq!(child.runtime_digest, "enhanced-digest");

    let child_item = json!({
        "method": "item/started",
        "params": {
            "threadId": "child-thread",
            "turnId": "child-turn",
            "item": {"type": "agentMessage", "id": "message-1", "text": "working"}
        }
    });
    assert_eq!(
        state
            .on_child_line(ExecutionPlane::EnhancedCodex, &child_item.to_string())
            .unwrap(),
        vec![BridgeAction::ToClient(child_item)]
    );

    let read = state
        .on_client_line(
            &json!({"id": 9, "method": "thread/read", "params": {"threadId": "child-thread"}})
                .to_string(),
        )
        .unwrap();
    assert!(matches!(
        &read[0],
        BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, _)
    ));

    // The Desktop child inspector may submit a follow-up directly to the
    // spawned thread. Binding only thread/read is not enough: the composer
    // uses the ordinary turn/start path, which must stay on the child's
    // inherited execution plane.
    let follow_up = json!({
        "id": 10,
        "method": "turn/start",
        "params": {
            "threadId": "child-thread",
            "input": [{"type": "text", "text": "Report current progress"}]
        }
    });
    assert_eq!(
        state
            .on_client_line(&follow_up.to_string())
            .expect("a direct child follow-up must remain routable"),
        vec![BridgeAction::ToChild(
            ExecutionPlane::EnhancedCodex,
            follow_up
        )]
    );
}

#[test]
fn legacy_subagent_lifecycle_also_binds_the_real_child_thread() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "parent-thread");
    let notification = json!({
        "method": "item/completed",
        "params": {
            "threadId": "parent-thread",
            "turnId": "parent-turn",
            "item": {
                "type": "subAgentActivity",
                "id": "spawn-call",
                "kind": "started",
                "agentThreadId": "child-thread",
                "agentPath": "/root/explore_project"
            },
            "completedAtMs": 42
        }
    });

    assert_eq!(
        state
            .on_child_line(ExecutionPlane::EnhancedCodex, &notification.to_string())
            .unwrap(),
        vec![BridgeAction::ToClient(notification)]
    );
    assert_eq!(
        state
            .store
            .get("child-thread")
            .unwrap()
            .unwrap()
            .provider_id,
        "qwen"
    );
}

#[test]
fn forged_spawn_sender_cannot_create_a_child_binding() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "parent-thread");
    let notification = json!({
        "method": "item/completed",
        "params": {
            "threadId": "parent-thread",
            "item": {
                "type": "collabAgentToolCall",
                "id": "spawn-call",
                "tool": "spawnAgent",
                "status": "completed",
                "senderThreadId": "different-thread",
                "receiverThreadIds": ["forged-child"],
                "agentsStates": {}
            }
        }
    });

    assert!(matches!(
        state.on_child_line(ExecutionPlane::EnhancedCodex, &notification.to_string()),
        Err(BridgeError::Protocol(_))
    ));
    assert!(state.store.get("forged-child").unwrap().is_none());
}

#[test]
fn an_enhanced_event_carrying_a_secret_field_is_rejected_not_recorded() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let event = json!({
        "method": ENHANCED_EVENT_NOTIFICATION,
        "params": {
            "name": "enhanced.tool.duplicate_suppressed",
            "fields": {"authorization": "Bearer secret"}
        }
    })
    .to_string();
    assert!(state
        .on_child_line(ExecutionPlane::EnhancedCodex, &event)
        .unwrap()
        .is_empty());
    assert!(state.journal.counts().is_empty());
    let journal = std::fs::read_to_string(temp.path().join("journal.jsonl")).unwrap();
    assert!(!journal.contains("Bearer secret"));
}

#[test]
fn only_legacy_resume_without_routing_hints_discovers_official() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);

    // A new thread never guesses Official when neither trusted routing
    // authority can classify it.
    let start_line = r#"{"id":1,"method":"thread/start","params":{"model":"unmapped-model"}}"#;
    assert!(matches!(
        state.on_client_line(start_line),
        Err(BridgeError::MissingRoutingAuthority)
    ));

    // A pre-bridge Official thread is the sole compatibility exception.
    let resume_line =
        r#"{"id":2,"method":"thread/resume","params":{"threadId":"thread-unbound-1"}}"#;
    let actions = state.on_client_line(resume_line).unwrap();
    assert!(matches!(
        &actions[0],
        BridgeAction::ToChild(ExecutionPlane::OfficialCodex, _)
    ));

    // Failed discovery is not an immutable binding.
    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            r#"{"id":2,"error":{"code":-32000,"message":"thread not found"}}"#,
        )
        .unwrap();
    assert!(state.store.get("thread-unbound-1").unwrap().is_none());

    let unknown_model_resume = r#"{"id":3,"method":"thread/resume","params":{"threadId":"thread-unbound-2","model":"unknown-third-party"}}"#;
    assert!(matches!(
        state.on_client_line(unknown_model_resume),
        Err(BridgeError::MissingRoutingAuthority)
    ));
}

#[test]
fn mapped_third_party_start_without_model_provider_routes_enhanced() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let model = "mapped-qwen".to_string();
    state.model_provider_map.models.insert(
        model.clone(),
        ModelProviderRoute {
            provider_id: "qwen".into(),
            child_provider_id: "vellum".into(),
        },
    );
    let line = json!({"id": 8, "method": "thread/start", "params": {"model": model}});
    let actions = state.on_client_line(&line.to_string()).unwrap();
    assert!(matches!(
        &actions[0],
        BridgeAction::ToChild(ExecutionPlane::EnhancedCodex, _)
    ));
}

/// Desktop naming the built-in `openai` provider still lands on the Official
/// plane, but never on the built-in table: that table reaches ChatGPT with the
/// Codex install's own `auth.json`, so a turn on it would ignore the account
/// Vellum has selected and stay invisible to usage and quota.
#[test]
fn start_with_openai_model_provider_routes_to_official_through_vellum() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    let line =
        r#"{"id":1,"method":"thread/start","params":{"modelProvider":"openai","model":"gpt-5.5"}}"#;
    let actions = state.on_client_line(line).unwrap();
    assert_eq!(
        actions[0],
        BridgeAction::ToChild(
            ExecutionPlane::OfficialCodex,
            json!({"id": 1, "method": "thread/start", "params": {"model": "gpt-5.5", "modelProvider": "vellum-official"}})
        )
    );
}

/// The bug this exists to stop: a thread created seconds before a restart, on
/// the Official plane, so Vellum's own in-flight counter read zero and the
/// restart went ahead. What the bridge sees has to reach the attestation.
#[test]
fn a_streaming_turn_is_open_from_the_request_until_the_completed_notification() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "thread-1");
    assert_eq!(state.attestation.snapshot().open_turns, 0);

    state
        .on_client_line(
            &json!({"id": 2, "method": "turn/start", "params": {"threadId": "thread-1"}})
                .to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 1);
    assert!(state.attestation.snapshot().has_open_turn());

    // `turn/start` only acknowledges. The turn is still running.
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            &json!({"id": 2, "result": {}}).to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 1);

    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            &json!({"method": "turn/completed", "params": {"threadId": "thread-1"}}).to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 0);
}

/// `turn/create` carries the whole turn in its response, so that response is
/// the end of it and there is no notification to wait for.
#[test]
fn a_blocking_turn_closes_when_its_own_response_arrives() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "thread-1");
    state
        .on_client_line(
            &json!({"id": 2, "method": "turn/create", "params": {"threadId": "thread-1"}})
                .to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 1);
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            &json!({"id": 2, "result": {"turn": {"status": "completed"}}}).to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 0);
}

/// A turn that was refused never ran. Counting it would refuse every restart
/// from then on, which is a worse failure than the one being fixed.
#[test]
fn a_refused_turn_does_not_leave_the_bridge_looking_busy() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "thread-1");
    state
        .on_client_line(
            &json!({"id": 2, "method": "turn/start", "params": {"threadId": "thread-1"}})
                .to_string(),
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::EnhancedCodex,
            &json!({"id": 2, "error": {"code": -32000, "message": "refused"}}).to_string(),
        )
        .unwrap();
    assert_eq!(state.attestation.snapshot().open_turns, 0);
}

/// A dead child cannot still be mid-turn, but the other plane can.
#[test]
fn a_dead_child_takes_only_its_own_turns_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp);
    start_enhanced_thread(&mut state, "qwen", "qwen3-coder", "thread-1");
    state
        .on_client_line(
            &json!({"id": 5, "method": "thread/start", "params": {"modelProvider": "openai", "model": "gpt-5.6-sol"}})
                .to_string(),
        )
        .unwrap();
    state
        .on_child_line(
            ExecutionPlane::OfficialCodex,
            &json!({"id": 5, "result": {"thread": {"id": "thread-official"}}}).to_string(),
        )
        .unwrap();
    for (id, thread) in [(6, "thread-1"), (7, "thread-official")] {
        state
            .on_client_line(
                &json!({"id": id, "method": "turn/start", "params": {"threadId": thread}})
                    .to_string(),
            )
            .unwrap();
    }
    assert_eq!(state.attestation.snapshot().open_turns, 2);

    state.mark_child_exited(ExecutionPlane::EnhancedCodex, "died");
    assert_eq!(
        state.attestation.snapshot().open_turns,
        1,
        "the Official turn is still running"
    );
}
