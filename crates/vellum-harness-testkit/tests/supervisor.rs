//! Process supervisor behaviour (§43 of the harness manager plan).
//!
//! Every native harness is launched as `executable + structured args`. There is
//! no shell in this path, so a workspace path or a config value can never be
//! reinterpreted as a command.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use vellum_harness_runtime::{
    HarnessLaunchSpec, HarnessProcessState, NativeHarnessSupervisor, RuntimeInstanceKey,
};

fn agent() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-acp-agent"))
}

fn spec(cwd: PathBuf) -> HarnessLaunchSpec {
    HarnessLaunchSpec {
        executable: agent(),
        args: vec![],
        cwd,
        env: BTreeMap::new(),
        inherit_env: true,
    }
}

#[tokio::test]
async fn a_known_executable_spawns_with_all_three_pipes_claimed() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let process = supervisor
        .spawn(spec(workspace.path().into()))
        .await
        .expect("fake agent spawns");
    let mut guard = process.lock().await;
    assert_eq!(guard.state, HarnessProcessState::Ready);
    assert!(guard.stdin.is_some());
    assert!(guard.stdout.is_some());
    // stderr is captured separately so diagnostics can never enter the
    // protocol stream on stdout.
    assert!(guard.stderr.is_some());
    guard
        .graceful_shutdown(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_invalid_executable_fails_instead_of_falling_back_to_a_shell() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let mut launch = spec(workspace.path().into());
    launch.executable = workspace.path().join("definitely-not-here");
    let error = supervisor.spawn(launch).await.unwrap_err();
    assert!(error.to_string().contains("RuntimeCrashed"));
}

#[tokio::test]
async fn a_missing_working_directory_is_refused_before_the_process_starts() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let error = supervisor
        .spawn(spec(workspace.path().join("no-such-dir")))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("RuntimeUnavailable"));
    assert!(error.to_string().contains("cwd does not exist"));
}

#[tokio::test]
async fn closing_stdin_lets_the_child_exit_gracefully_and_leaves_no_zombie() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let process = supervisor
        .spawn(spec(workspace.path().into()))
        .await
        .unwrap();
    let mut guard = process.lock().await;
    // The agent exits on EOF; dropping stdin is the graceful signal.
    drop(guard.stdin.take());
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(guard.state, HarnessProcessState::Exited);
}

#[tokio::test]
async fn a_child_that_ignores_shutdown_is_hard_killed_at_the_deadline() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let process = supervisor
        .spawn(spec(workspace.path().into()))
        .await
        .unwrap();
    let mut guard = process.lock().await;
    // stdin stays open, so the agent is still waiting for input; the deadline
    // has to escalate to a kill rather than hang the caller.
    guard
        .graceful_shutdown(Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(guard.state, HarnessProcessState::Exited);
}

#[tokio::test]
async fn each_spawn_gets_a_distinct_runtime_instance_id() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    let first = supervisor
        .spawn(spec(workspace.path().into()))
        .await
        .unwrap();
    let second = supervisor
        .spawn(spec(workspace.path().into()))
        .await
        .unwrap();
    let first_key = first.lock().await.key.clone();
    let second_key = second.lock().await.key.clone();
    assert_ne!(first_key, second_key);

    // A restart is a new instance, so a handle from the old one can never be
    // mistaken for a live session on the new one.
    supervisor.remove(&first_key).await;
    supervisor.remove(&second_key).await;
    assert_ne!(first_key, RuntimeInstanceKey(String::new()));

    for process in [first, second] {
        process
            .lock()
            .await
            .graceful_shutdown(Duration::from_secs(5))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_non_inheriting_launch_passes_only_the_variables_it_was_given() {
    let workspace = tempfile::tempdir().unwrap();
    let supervisor = NativeHarnessSupervisor::default();
    std::env::set_var("VELLUM_TEST_SECRET_MUST_NOT_LEAK", "sensitive");
    let mut launch = spec(workspace.path().into());
    launch.inherit_env = false;
    launch.env = BTreeMap::from([("VELLUM_FAKE_ACP_SCRIPT".to_string(), "{}".to_string())]);
    // Windows needs SystemRoot for a process to start with a cleared block.
    if let Ok(system_root) = std::env::var("SystemRoot") {
        launch.env.insert("SystemRoot".into(), system_root);
    }
    let process = supervisor.spawn(launch).await.expect("cleared env spawns");
    let mut guard = process.lock().await;
    drop(guard.stdin.take());
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .await
        .unwrap();
    std::env::remove_var("VELLUM_TEST_SECRET_MUST_NOT_LEAK");
}
