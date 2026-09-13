//! Portable / fork shared-module consistency gate.
//! Fork-only files (runtime.rs, seams.rs, reporting.rs, context_projection.rs,
//! debug_log.rs, mod.rs) are not compared and must not be overwritten.

use std::fs;
use std::path::PathBuf;

const SHARED_MODULES: &[&str] = &[
    "bounded_continuation.rs",
    "config.rs",
    "context_pruner.rs",
    "context_recovery.rs",
    "digest.rs",
    "gateway.rs",
    "hooks.rs",
    "notifications.rs",
    "telemetry.rs",
    "tool_observation.rs",
    "tool_reliability.rs",
];

const FORK_ONLY: &[&str] = &[
    "mod.rs",
    "runtime.rs",
    "seams.rs",
    "reporting.rs",
    "context_projection.rs",
    "debug_log.rs",
    "debug_log_tests.rs",
    "lockfile.rs",
];

fn portable_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn fork_enhanced() -> Result<PathBuf, String> {
    let root = std::env::var("VELLUM_ENHANCED_CORE_ROOT").map_err(|_| {
        "VELLUM_ENHANCED_CORE_ROOT is required; a missing fork checkout must fail this gate".to_string()
    })?;
    let path = PathBuf::from(root).join("codex-rs/core/src/enhanced");
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "enhanced-codex-core fork checkout is missing at {}",
            path.display()
        ))
    }
}

fn normalize(source: &str) -> String {
    source.replace("\r\n", "\n")
}

#[test]
fn shared_modules_match_the_committed_fork() {
    let fork = fork_enhanced().expect("missing fork checkout must fail");
    let portable = portable_src();
    let mut mismatches = Vec::new();
    for name in SHARED_MODULES {
        let left = fs::read_to_string(portable.join(name)).unwrap();
        let right_path = fork.join(name);
        assert!(
            right_path.is_file(),
            "fork missing shared module {}",
            right_path.display()
        );
        let right = fs::read_to_string(&right_path).unwrap();
        if normalize(&left) != normalize(&right) {
            mismatches.push(*name);
        }
    }
    assert!(
        mismatches.is_empty(),
        "shared modules drifted: {mismatches:?}"
    );
    for name in FORK_ONLY {
        assert!(
            fork.join(name).is_file() || *name == "lockfile.rs",
            "fork-only {} missing",
            name
        );
        if *name != "lockfile.rs" {
            assert!(
                !portable.join(name).is_file() || *name == "lockfile.rs",
                "portable crate must not grow fork-only {name}"
            );
        }
    }
    assert!(portable.join("lockfile.rs").is_file());
    assert!(fork.join("lockfile.rs").is_file());
}

#[test]
fn fork_only_names_are_not_in_the_shared_copy_list() {
    for name in FORK_ONLY {
        if *name == "lockfile.rs" {
            continue;
        }
        assert!(
            !SHARED_MODULES.contains(name),
            "{name} is fork-only and must not be overwritten by sync"
        );
    }
}

#[test]
fn portable_sources_exist() {
    let root = portable_src();
    for name in SHARED_MODULES {
        assert!(root.join(name).is_file(), "{}", name);
    }
}
