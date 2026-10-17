use super::*;

fn fixture() -> (tempfile::TempDir, AgentStateStore) {
    let temp = tempfile::tempdir().unwrap();
    let paths = AgentPaths::from_root(temp.path().join("state"));
    paths.ensure().unwrap();
    let binary = native_proxy_bin(&paths);
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(binary, b"fake-proxy").unwrap();
    let store = AgentStateStore::new(paths);
    let mut state = store.load().unwrap();
    state.install = Some(InstallRecord {
        install_id: "install".into(),
        host_id: state.host_id.clone(),
        image: String::new(),
        image_digest: None,
        config_hash: "config".into(),
        host_port: 15721,
        updated_at: chrono::Utc::now(),
        proxy_backend: Some("native".into()),
        native_executable_digest: None,
    });
    store.save(&state).unwrap();
    (temp, store)
}

#[test]
fn launchd_and_systemd_wait_until_the_nth_probe_is_ready() {
    for systemd in [false, true] {
        let (temp, store) = fixture();
        let service = MemoryLaunchd {
            systemd,
            ready_after: Mutex::new(Some(3)),
            ..Default::default()
        };
        let view = start_native(&store, &service, temp.path(), "user", None, "boundary").unwrap();
        assert!(view.ready && view.running);
        assert_eq!(service.probes.load(std::sync::atomic::Ordering::Relaxed), 3);
    }
}

#[test]
fn readyz_alone_does_not_satisfy_the_runtime_contract() {
    let (temp, store) = fixture();
    let service = MemoryLaunchd {
        ready: Mutex::new(true),
        websocket: Mutex::new(Some(false)),
        ..Default::default()
    };
    let error = start_native(&store, &service, temp.path(), "user", None, "boundary").unwrap_err();
    assert!(error.contains("NativeProxyRuntimeNotReady"));
    assert!(service.probes.load(std::sync::atomic::Ordering::Relaxed) > 1);
    assert!(
        !observe_native(&store, &service, temp.path(), "user", "boundary")
            .unwrap()
            .ready
    );
}

#[test]
fn failed_unit_or_disappearing_main_pid_stops_probing_early() {
    for lost_pid in [false, true] {
        let (temp, store) = fixture();
        let service = MemoryLaunchd::default();
        if lost_pid {
            *service.pid_gone_after.lock().unwrap() = Some(2);
        } else {
            *service.fail_after.lock().unwrap() = Some(2);
        }
        let error =
            start_native(&store, &service, temp.path(), "user", None, "boundary").unwrap_err();
        assert!(error.contains("NativeProxyServiceExited"));
        assert_eq!(service.probes.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
