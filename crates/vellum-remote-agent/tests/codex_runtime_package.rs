//! Coordinator harness supplies the real, publisher-digest-verified package.
use vellum_remote_agent::{codex_package, ServiceReconcileResult};

#[test]
#[ignore = "requires VELLUM_CODEX_PACKAGE and VELLUM_CODEX_PACKAGE_SHA256 from the Linux harness"]
fn installs_official_runtime_package_from_harness() {
    let archive = std::path::PathBuf::from(
        std::env::var_os("VELLUM_CODEX_PACKAGE").expect("official package archive"),
    );
    let digest = std::env::var("VELLUM_CODEX_PACKAGE_SHA256")
        .expect("publisher SHA-256 from release metadata");
    let home = tempfile::tempdir().unwrap();
    vellum_remote_agent::native_codex::install_pinned_codex_with(
        home.path(),
        &archive,
        &digest,
        "0.147.0-alpha.6.6",
        |_| Some("codex-cli 0.147.0-alpha.6.6".into()),
        |_| Ok(ServiceReconcileResult::default()),
    )
    .unwrap();
    let current = home.path().join("packages/standalone/current");
    assert!(codex_package::inspect_installed(&current).complete);
    assert!(current.join("bin/codex-code-mode-host").is_file());
    assert!(current.join("codex-path/rg").is_file());
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(current.join("codex")).unwrap(),
        std::path::Path::new("bin/codex")
    );
}
