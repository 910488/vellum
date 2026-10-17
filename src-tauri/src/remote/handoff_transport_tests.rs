use super::*;
use crate::remote::discovery::RemoteHostCandidate;
use crate::remote::ssh_isolation::{has_managed_alias, managed_alias, ManagedSshConfig};

fn fixture(host: &str, alias: &str) -> (tempfile::TempDir, AppState, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().join("data"));
    state
        .remote()
        .import_discovered_host(&RemoteHostCandidate {
            codex_host_id: None,
            vellum_host_id: host.into(),
            display_name: alias.into(),
            ssh_alias: alias.into(),
            hostname: Some("host.test".into()),
            user: Some("user".into()),
            port: Some(22),
            source: "sshConfig".into(),
            validated: true,
            validation_error: None,
        })
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    (temp, state, home)
}

fn block(host: &str) -> String {
    ManagedSshConfig {
        host_id: host.into(),
        alias: managed_alias(host),
        hostname: "host.test".into(),
        user: "user".into(),
        port: 22,
        identity_file: "fixture-id_ed25519".into(),
    }
    .block()
}

#[test]
fn discovered_host_uses_installed_managed_alias_without_changing_cache() {
    let (_temp, state, home) = fixture("remote-host", "original-alias");
    let config = home.join(".ssh/config");
    let contents = format!(
        "Host original-alias\n HostName host.test\n{}",
        block("remote-host")
    );
    fs::write(&config, &contents).unwrap();
    let transport = transport_with_home(&state, "remote-host", Some(&home)).unwrap();
    assert_eq!(
        transport.client.target().ssh_destination.as_deref(),
        Some("vellum-remote-remote-host")
    );
    // The client used by RPC is also the source of push/fetch's SSH destination.
    assert_eq!(transport.root, state.data_root());
    assert_eq!(
        RemoteHostManager::resolve_target(&state, "remote-host")
            .unwrap()
            .ssh_destination
            .as_deref(),
        Some("original-alias")
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), contents);
}

#[test]
fn discovered_host_refuses_missing_unreadable_unmarked_foreign_or_incomplete_blocks() {
    let (_temp, state, home) = fixture("remote-host", "original-alias");
    let config = home.join(".ssh/config");
    let assert_refused = || {
        let error = transport_with_home(&state, "remote-host", Some(&home))
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("HandoffManagedSshRequired:"));
    };
    assert_refused(); // no file
    for contents in [
        "Host vellum-remote-remote-host\n HostName host.test\n".into(),
        block("another-host"),
        block("another-host").replace("vellum-remote-another-host", "vellum-remote-remote-host"),
        block("remote-host").replace("# vellum-remote-managed:end remote-host\n", ""),
        block("remote-host").replace("Host vellum-remote-remote-host", "Host original-alias"),
    ] {
        fs::write(&config, contents).unwrap();
        assert_refused();
    }
    fs::write(&config, [0xff]).unwrap();
    assert_refused();
    assert!(transport_with_home(&state, "remote-host", None).is_err());
}

#[test]
fn cached_managed_alias_alone_does_not_prove_isolation() {
    let (_temp, state, home) = fixture("remote-host", &managed_alias("remote-host"));
    assert!(transport_with_home(&state, "remote-host", Some(&home)).is_err());
}

#[test]
fn added_host_keeps_private_config_and_existing_managed_cache_alias() {
    let host = "added-fixture";
    let alias = managed_alias(host);
    let (_temp, state, home) = fixture(host, &alias);
    // An added host must never use a lookalike block in the user's config.
    fs::write(home.join(".ssh/config"), block(host)).unwrap();
    assert!(transport_with_home(&state, host, Some(&home)).is_err());
    let config = crate::remote::ssh_added::config_path(&state.data_root());
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, block(host)).unwrap();
    let transport = transport_with_home(&state, host, None).unwrap();
    assert_eq!(
        transport.client.target().ssh_destination.as_deref(),
        Some(alias.as_str())
    );
    assert_eq!(
        RemoteHostManager::resolve_target(&state, host)
            .unwrap()
            .ssh_destination
            .as_deref(),
        Some(alias.as_str())
    );
    assert_eq!(
        crate::remote::ssh_added::config_args(&state.data_root(), &alias),
        vec![
            "-F".to_string(),
            config.to_string_lossy().replace('\\', "/")
        ]
    );
}

#[test]
fn managed_block_matching_refuses_duplicate_or_ambiguous_host_scopes() {
    let valid = block("remote-host");
    assert!(has_managed_alias(&valid, "remote-host"));
    for contents in [
        format!("{valid}{valid}"),
        valid.replace(
            "Host vellum-remote-remote-host",
            "Host vellum-remote-remote-host other",
        ),
        valid.replace("    HostName", "Host other\n    HostName"),
        valid.replace(
            "    HostName",
            "# vellum-remote-managed:begin other\n    HostName",
        ),
    ] {
        assert!(!has_managed_alias(&contents, "remote-host"));
    }
}
