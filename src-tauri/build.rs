use sha2::{Digest, Sha256};

fn main() {
    println!(
        "cargo:rustc-env=VELLUM_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    record_bridge_sidecar();
    record_enhanced_runtime();
    let relay = format!(
        "binaries/vellum-codex-relay{}",
        if std::env::var("TARGET")
            .unwrap_or_default()
            .contains("windows")
        {
            ".exe"
        } else {
            ""
        }
    );
    println!("cargo:rerun-if-changed={relay}");
    let relay_digest = std::fs::read(&relay)
        .ok()
        .map(|bytes| format!("sha256:{:x}", Sha256::digest(bytes)))
        .unwrap_or_default();
    println!("cargo:rustc-env=VELLUM_BUNDLED_RELAY_SHA256={relay_digest}");
    tauri_build::build()
}

/// Records the identity of the packaged App Server bridge.
///
/// `CODEX_CLI_PATH` points Codex Desktop at this file, so a release has to be
/// able to say which bytes it shipped. The sidecar is staged by
/// `scripts/build-sidecar.mjs` before the app compiles; when it is absent (a
/// plain `cargo build`, or a check-only run) the value is empty rather than
/// wrong, and `packaged_bridge_executable()` reports the gap at runtime.
fn record_bridge_sidecar() {
    let suffix = if std::env::var("TARGET")
        .unwrap_or_default()
        .contains("windows")
    {
        ".exe"
    } else {
        ""
    };
    let pointer = std::path::PathBuf::from("binaries/vellum-codex-app-server.dev-path");
    println!("cargo:rerun-if-changed={}", pointer.display());
    let relative = if std::env::var("PROFILE").as_deref() == Ok("debug") {
        std::fs::read_to_string(&pointer)
            .ok()
            .map(|value| value.trim().replace('\\', "/"))
            .filter(|value| value.starts_with("binaries/dev/") && !value.contains(".."))
            .unwrap_or_default()
    } else {
        format!("binaries/vellum-codex-app-server{suffix}")
    };
    let staged = std::path::PathBuf::from(&relative);
    println!("cargo:rerun-if-changed={}", staged.display());
    let digest = std::fs::read(&staged)
        .map(|bytes| format!("sha256:{:x}", Sha256::digest(bytes)))
        .unwrap_or_default();
    println!("cargo:rustc-env=VELLUM_BUNDLED_BRIDGE_SHA256={digest}");
    println!("cargo:rustc-env=VELLUM_BUNDLED_BRIDGE_RELATIVE_PATH={relative}");
}

/// Records where the packaged Enhanced Codex core lives.
///
/// Unlike the bridge, no digest is recorded here. The Enhanced core's identity
/// is already pinned by `enhanced-runtime.lock.json` (`artifactSha256`), which
/// `verify_settings` checks against the real file before Enhanced can be armed
/// — a second digest baked in at build time could only ever agree or contradict
/// the lockfile, and the lockfile is the one the qualification journal cites.
///
/// A dev build may write a pointer at
/// `binaries/vellum-enhanced-codex.dev-path`, because the core is a ~300 MB
/// build output that nobody wants copied on every rebuild. A release records
/// the legacy bundled location for upgrade compatibility, but new Desktop
/// packages do not include that file; signed managed slots are authoritative.
fn record_enhanced_runtime() {
    let suffix = if std::env::var("TARGET")
        .unwrap_or_default()
        .contains("windows")
    {
        ".exe"
    } else {
        ""
    };
    let pointer = std::path::PathBuf::from("binaries/vellum-enhanced-codex.dev-path");
    println!("cargo:rerun-if-changed={}", pointer.display());
    let relative = if std::env::var("PROFILE").as_deref() == Ok("debug") {
        std::fs::read_to_string(&pointer)
            .ok()
            .map(|value| value.trim().replace('\\', "/"))
            .filter(|value| !value.is_empty() && !value.contains(".."))
            .unwrap_or_default()
    } else {
        format!("binaries/vellum-enhanced-codex{suffix}")
    };
    println!("cargo:rustc-env=VELLUM_BUNDLED_ENHANCED_RELATIVE_PATH={relative}");
}
