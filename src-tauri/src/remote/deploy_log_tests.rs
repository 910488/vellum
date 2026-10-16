use super::*;
use serde_json::json;

fn progress(id: &str) -> RemoteOperationProgress {
    RemoteOperationProgress::queued(id.into(), "host".into(), "oneClickBootstrap")
}
fn complete(root: &Path, id: &str, state: &str, message: Option<&str>, trigger: Trigger) {
    let mut p = progress(id);
    super::super::operation::save(root, &p).unwrap();
    let mut recorder = Recorder::begin(
        root,
        &p,
        trigger,
        Some(json!({"status": {"capabilities": {
            "os": "linux", "arch": "aarch64", "persistenceScope": "onDemand"
        }}})),
    )
    .unwrap();
    recorder.phase("hostPreflight");
    recorder.phase("deploymentApply");
    p.state = state.into();
    p.message = message.map(str::to_string);
    super::super::operation::save(root, &p).unwrap();
    recorder.finish(&p).unwrap();
}

#[test]
fn converge_outcomes_produce_two_lines_and_count_blocked_separately_from_failures() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    complete(root, "success", "completed", None, Trigger::Deploy);
    complete(
        root,
        "failed",
        "failed",
        Some("proxyPortConflict: busy"),
        Trigger::Redeploy,
    );
    complete(
        root,
        "blocked",
        "failed",
        Some("OneClickBootstrapBlocked: psStartTimeUnavailable; repair: apk add procps-ng"),
        Trigger::Models,
    );
    complete(
        root,
        "cancelled",
        "cancelled",
        Some("DeploymentCancelled"),
        Trigger::Restore,
    );
    let events = history(root, "host").unwrap();
    assert_eq!(events.len(), 8);
    for pair in events.as_chunks::<2>().0 {
        assert_eq!(pair[0].event, "started");
        assert_eq!(pair[1].event, "finished");
        assert_eq!(pair[0].operation_id, pair[1].operation_id);
        assert_eq!(pair[1].phases.len(), 3);
        assert_eq!(pair[1].metadata.os.as_deref(), Some("linux"));
        assert_eq!(pair[1].metadata.arch.as_deref(), Some("aarch64"));
        assert_eq!(
            pair[1].metadata.persistence_scope.as_deref(),
            Some("onDemand")
        );
        assert!(!pair[1].build.is_empty());
    }
    assert_eq!(events[3].result, Some(Outcome::Failed));
    assert_eq!(events[5].result, Some(Outcome::Blocked));
    assert_eq!(events[7].result, Some(Outcome::Cancelled));
    let summary = summary(root, "host").unwrap();
    assert_eq!(
        (
            summary.total,
            summary.succeeded,
            summary.failed,
            summary.blocked,
            summary.interrupted
        ),
        (2, 1, 1, 1, 0)
    );
    assert_eq!(
        summary.last_failure_code.as_deref(),
        Some("psStartTimeUnavailable")
    );
}

#[test]
fn crash_reconciles_only_after_the_live_owner_disappears_and_only_once() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let id = format!("crash-{}", ulid::Ulid::new());
    let mut p = progress(&id);
    let _control = super::super::converge_control::register(&id, "host").unwrap();
    let registration = super::super::converge_control::Registration(id);
    super::super::operation::save(root, &p).unwrap();
    let recorder = Recorder::begin(root, &p, Trigger::Deploy, None).unwrap();
    assert_eq!(history(root, "host").unwrap().len(), 1);
    drop(recorder);
    drop(registration);
    let events = history(root, "host").unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].result, Some(Outcome::Interrupted));
    assert_eq!(history(root, "host").unwrap().len(), 2);
    assert_eq!(summary(root, "host").unwrap().interrupted, 1);
    // Even if the operation file vanished, the durable started event remains evidence.
    p.operation_id = "missing-operation".into();
    let _lost = Recorder::begin(root, &p, Trigger::Restore, None).unwrap();
    assert_eq!(history(root, "host").unwrap().len(), 4);
    assert_eq!(history(root, "host").unwrap().len(), 4);
}

#[test]
fn bounded_history_keeps_exactly_500_whole_operations_and_support_exports_latest_20_pairs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for i in 0..502 {
        complete(
            root,
            &format!("op-{i}"),
            "completed",
            None,
            Trigger::Redeploy,
        );
    }
    let events = history(root, "host").unwrap();
    assert_eq!(events.len(), 1000);
    assert_eq!(events[0].operation_id, "op-2");
    assert_eq!(events[999].operation_id, "op-501");
    let latest = recent(root, "host").unwrap();
    assert_eq!(latest.len(), 40);
    assert_eq!(latest[0].operation_id, "op-482");
    assert_eq!(summary(root, "host").unwrap().succeeded, 500);
    assert!(!fs::read_dir(root.join("remote-deploy-log"))
        .unwrap()
        .flatten()
        .any(|e| e.path().to_string_lossy().contains("tmp-")));
}

#[test]
fn errors_metadata_and_phases_never_copy_secrets_identifiers_or_raw_text_into_the_log() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut p = progress("privacy");
    let private = [
        "sk-secret-value",
        "token-grant",
        "operator42",
        "192.0.2.10",
        "15721",
        "/home/operator42/.codex",
        "C:\\Users\\operator42",
        "raw-error-sentinel",
    ];
    let message = format!("psStartTimeUnavailable: {}", private.join(" "));
    let mut recorder = Recorder::begin(
        root,
        &p,
        Trigger::Models,
        Some(json!({"status": {"capabilities": {
            "os": message, "arch": message, "persistenceScope": message
        }}})),
    )
    .unwrap();
    recorder.phase(&message);
    p.state = "failed".into();
    p.message = Some(message);
    p.result = Some(json!({"token": private, "os": "sk-secret-value"}));
    recorder.finish(&p).unwrap();
    let text = fs::read_to_string(path(root, "host").unwrap()).unwrap();
    for value in private {
        assert!(!text.contains(value), "leaked {value}");
    }
    assert!(text.contains("psStartTimeUnavailable"));
    assert_eq!(
        failure_code("raw-error-sentinel operator42"),
        "DeploymentFailed"
    );
    assert_eq!(
        failure_code("prefixpsStartTimeUnavailableSuffix"),
        "DeploymentFailed"
    );
}

#[test]
fn unreachable_remote_still_exports_local_log_and_redacted_operation_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    complete(
        root,
        "failed",
        "failed",
        Some("ssh: operator42@192.0.2.10 /home/operator42 token-grant"),
        Trigger::Deploy,
    );
    // Older operation without a deployment log still belongs in the local bundle.
    let mut old = progress("old-operation");
    old.state = "completed".into();
    super::super::operation::save(root, &old).unwrap();
    let value = support_bundle(root, "host", || {
        Err(AppError::Message("ssh: connection refused".into()))
    })
    .unwrap();
    assert_eq!(value["remoteUnavailable"], true);
    assert_eq!(
        value["bundle"]["deploymentLog"].as_array().unwrap().len(),
        2
    );
    assert_eq!(value["bundle"]["operations"].as_array().unwrap().len(), 2);
    let saved = fs::read_to_string(value["path"].as_str().unwrap()).unwrap();
    assert!(!saved.contains("operator42"));
    assert!(!saved.contains("token-grant"));
    assert!(value["bundle"]["remote"].is_null());
    let online =
        support_bundle(root, "host", || Ok(json!({"path": "remote-redacted.tar"}))).unwrap();
    assert_eq!(online["remoteUnavailable"], false);
    assert_eq!(online["bundle"]["remote"]["path"], "remote-redacted.tar");
}

#[test]
fn invalid_ids_refuse_before_io_and_torn_append_is_repaired_before_finishing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for id in ["../escape", "", "a/b", "a\\b", "C:host", "..／escape"] {
        assert!(history(root, id).is_err());
    }
    assert!(!root.join("remote-deploy-log").exists());
    let mut p = progress("torn");
    let recorder = Recorder::begin(root, &p, Trigger::Deploy, None).unwrap();
    OpenOptions::new()
        .append(true)
        .open(path(root, "host").unwrap())
        .unwrap()
        .write_all(b"{\"event\":")
        .unwrap();
    p.state = "completed".into();
    recorder.finish(&p).unwrap();
    assert_eq!(history(root, "host").unwrap().len(), 2);
}

#[test]
fn concurrent_reconciliation_appends_only_one_interrupted_event() {
    let temp = tempfile::tempdir().unwrap();
    let p = progress("parallel");
    Recorder::begin(temp.path(), &p, Trigger::Deploy, None).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                history(temp.path(), "host").unwrap();
            });
        }
    });
    assert_eq!(history(temp.path(), "host").unwrap().len(), 2);
}

#[test]
fn a_successful_retry_keeps_the_initial_verification_failure_and_measures_phase_time() {
    let temp = tempfile::tempdir().unwrap();
    let mut p = progress("retry");
    let mut recorder = Recorder::begin(temp.path(), &p, Trigger::Deploy, None).unwrap();
    recorder.phase("verification");
    recorder.tick = Instant::now() - std::time::Duration::from_millis(37);
    recorder.phase("verificationRetry");
    p.state = "completed".into();
    p.result =
        Some(json!({"bootstrap": {"os": "linux", "arch": "x86_64", "persistenceScope": "login"}}));
    recorder.finish(&p).unwrap();
    let events = history(temp.path(), "host").unwrap();
    let event = &events[1];
    assert_eq!(event.phases[1].phase, "verification");
    assert_eq!(event.phases[1].result, Outcome::Failed);
    assert!(event.phases[1].ms >= 37);
    assert_eq!(event.phases[2].result, Outcome::Succeeded);
    assert_eq!(event.result, Some(Outcome::Succeeded));
    assert_eq!(event.metadata.persistence_scope.as_deref(), Some("login"));
}

#[test]
fn repairing_an_old_crash_does_not_hide_the_latest_success() {
    let temp = tempfile::tempdir().unwrap();
    Recorder::begin(temp.path(), &progress("older-crash"), Trigger::Deploy, None).unwrap();
    complete(
        temp.path(),
        "latest-success",
        "completed",
        None,
        Trigger::Redeploy,
    );
    let summary = summary(temp.path(), "host").unwrap();
    assert_eq!(summary.interrupted, 1);
    assert_eq!(summary.succeeded, 1);
    assert!(summary.last_failure_code.is_none());
}
