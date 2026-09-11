//! Where things live on disk: the files this Vellum install shipped, and the
//! Codex CLI that Codex Desktop installed.
//!
//! Every lookup used to carry its own copy of the Windows layout, where a
//! Tauri bundle's resources sit beside the executable. A macOS `.app` keeps
//! them in `Contents/Resources`, one directory over from `Contents/MacOS`, and
//! each copy missed that on its own: a 0.2.6 DMG that contained both the bridge
//! and the Enhanced core reported both missing. The layout is learned here,
//! once.
//!
//! The `*_in`/`*_for` forms take their inputs so the macOS layout is testable
//! on any host.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Directories a Tauri bundle may put its resources under, for an executable
/// in `exe_dir`: always `exe_dir` itself, and for a macOS app bundle
/// (`…/Contents/MacOS`) also `…/Contents/Resources`.
pub fn resource_roots_for(exe_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![exe_dir.to_path_buf()];
    let contents = exe_dir
        .parent()
        .filter(|_| exe_dir.file_name() == Some(OsStr::new("MacOS")))
        .filter(|contents| contents.file_name() == Some(OsStr::new("Contents")));
    if let Some(contents) = contents {
        roots.push(contents.join("Resources"));
    }
    roots
}

/// Every place a file shipped under the bundle's `binaries/` resource can be.
/// The executable's own directory comes first because a dev build leaves
/// sidecars there; callers that care which copy they get decide by hash.
pub fn binary_candidates_in(exe_dir: &Path, name: &OsStr) -> Vec<PathBuf> {
    let mut candidates = vec![exe_dir.join(name)];
    for root in resource_roots_for(exe_dir) {
        candidates.push(root.join("binaries").join(name));
        candidates.push(root.join("resources").join("binaries").join(name));
    }
    candidates
}

/// Every directory the bundled Linux remote payload (`resources/remote`) can
/// be in.
pub fn remote_roots_in(exe_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in resource_roots_for(exe_dir) {
        roots.push(root.join("resources").join("remote"));
        roots.push(root.join("remote"));
    }
    roots
}

fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// [`binary_candidates_in`] for the running executable.
pub fn bundled_binary_candidates(name: impl AsRef<OsStr>) -> Vec<PathBuf> {
    executable_dir()
        .map(|directory| binary_candidates_in(&directory, name.as_ref()))
        .unwrap_or_default()
}

/// [`remote_roots_in`] for the running executable, then the source tree's
/// copy so `cargo run` finds what the build staged.
pub fn remote_resource_roots() -> Vec<PathBuf> {
    let mut roots = executable_dir()
        .map(|directory| remote_roots_in(&directory))
        .unwrap_or_default();
    roots.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("remote"),
    );
    roots
}

/// Codex Desktop's app bundle on macOS, most likely first. It installs as
/// `ChatGPT.app` (bundle id `com.openai.codex`); `Codex.app` covers installs
/// that still carry the older name, and `~/Applications` a per-user install.
pub fn macos_codex_app_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut apps = Vec::new();
    for name in ["ChatGPT.app", "Codex.app"] {
        apps.push(Path::new("/Applications").join(name));
        if let Some(home) = home {
            apps.push(home.join("Applications").join(name));
        }
    }
    apps
}

/// Codex Desktop's own executable on macOS, `<app>/Contents/MacOS/<app name>`
/// for each of [`macos_codex_app_candidates`]. This is the process that
/// spawns the app server, so a bridge whose parent is one of these was
/// adopted by Desktop.
pub fn macos_codex_desktop_executable_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    macos_codex_app_candidates(home)
        .into_iter()
        .filter_map(|app| {
            let stem = app.file_stem()?.to_owned();
            Some(app.join("Contents").join("MacOS").join(stem))
        })
        .collect()
}

/// The Codex CLI that belongs to Codex Desktop itself: each app bundle's
/// `Contents/Resources/codex`, then the copy Desktop keeps for plugin app
/// servers under `~/.codex`. These speak the protocol Desktop speaks.
pub fn macos_codex_desktop_cli_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = macos_codex_app_candidates(home)
        .into_iter()
        .map(|app| app.join("Contents").join("Resources").join("codex"))
        .collect();
    if let Some(home) = home {
        candidates.push(
            home.join(".codex")
                .join("plugins")
                .join(".plugin-appserver")
                .join("codex"),
        );
    }
    candidates
}

/// Every Codex CLI worth trying on macOS, Desktop's own first. A GUI app
/// inherits launchd's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so the
/// package-manager locations a shell would find must be named here or they
/// are never seen.
pub fn macos_codex_cli_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = macos_codex_desktop_cli_candidates(home);
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    if let Some(home) = home {
        candidates.push(home.join(".local").join("bin").join("codex"));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_its_own_only_resource_root() {
        let directory = Path::new("C:/Program Files/Vellum");
        assert_eq!(resource_roots_for(directory), vec![directory.to_path_buf()]);
    }

    #[test]
    fn a_macos_app_bundle_also_looks_in_contents_resources() {
        let exe_dir = Path::new("/Applications/Vellum.app/Contents/MacOS");
        assert_eq!(
            resource_roots_for(exe_dir),
            vec![
                exe_dir.to_path_buf(),
                Path::new("/Applications/Vellum.app/Contents").join("Resources"),
            ]
        );
        // A directory merely named MacOS is not a bundle.
        assert_eq!(resource_roots_for(Path::new("/tmp/MacOS")).len(), 1);
    }

    #[test]
    fn the_windows_layout_keeps_its_order() {
        let exe_dir = Path::new("C:/Vellum");
        let name = OsStr::new("vellum-codex-app-server.exe");
        assert_eq!(
            binary_candidates_in(exe_dir, name),
            vec![
                exe_dir.join(name),
                exe_dir.join("binaries").join(name),
                exe_dir.join("resources").join("binaries").join(name),
            ]
        );
    }

    /// The 0.2.6 DMG: the bridge sat in `Contents/Resources/binaries` and every
    /// lookup, searching only beside `Contents/MacOS/<exe>`, said it was missing.
    #[test]
    fn a_sidecar_inside_a_macos_bundle_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("Vellum.app").join("Contents");
        let exe_dir = contents.join("MacOS");
        let shipped = contents
            .join("Resources")
            .join("binaries")
            .join("vellum-codex-app-server");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(shipped.parent().unwrap()).unwrap();
        std::fs::write(&shipped, b"bridge").unwrap();

        let found = binary_candidates_in(&exe_dir, OsStr::new("vellum-codex-app-server"))
            .into_iter()
            .find(|candidate| candidate.is_file());
        assert_eq!(found, Some(shipped));
    }

    #[test]
    fn the_remote_payload_is_found_inside_a_macos_bundle() {
        let exe_dir = Path::new("/Applications/Vellum.app/Contents/MacOS");
        assert!(remote_roots_in(exe_dir).contains(
            &Path::new("/Applications/Vellum.app/Contents")
                .join("Resources")
                .join("resources")
                .join("remote")
        ));
    }

    #[test]
    fn codex_desktop_is_tried_before_any_package_manager_cli() {
        let home = Path::new("/Users/someone");
        let desktop = macos_codex_desktop_cli_candidates(Some(home));
        let all = macos_codex_cli_candidates(Some(home));

        assert_eq!(
            all[0],
            Path::new("/Applications")
                .join("ChatGPT.app")
                .join("Contents")
                .join("Resources")
                .join("codex")
        );
        assert_eq!(&all[..desktop.len()], &desktop[..]);
        assert!(desktop.contains(
            &home
                .join(".codex")
                .join("plugins")
                .join(".plugin-appserver")
                .join("codex")
        ));
        assert!(all[desktop.len()..].contains(&PathBuf::from("/opt/homebrew/bin/codex")));
        assert!(!desktop.contains(&PathBuf::from("/opt/homebrew/bin/codex")));
    }

    #[test]
    fn desktop_executables_are_named_after_their_app_bundle() {
        let home = Path::new("/Users/someone");
        let executables = macos_codex_desktop_executable_candidates(Some(home));
        assert_eq!(
            executables.len(),
            macos_codex_app_candidates(Some(home)).len()
        );
        assert_eq!(
            executables[0],
            Path::new("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT")
        );
        assert!(executables.contains(
            &home
                .join("Applications")
                .join("Codex.app")
                .join("Contents")
                .join("MacOS")
                .join("Codex")
        ));
    }

    #[test]
    fn without_a_home_only_system_locations_remain() {
        assert_eq!(macos_codex_app_candidates(None).len(), 2);
        assert!(macos_codex_cli_candidates(None)
            .iter()
            .all(|candidate| candidate.has_root()));
    }
}
