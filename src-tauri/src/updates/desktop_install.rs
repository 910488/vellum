//! Close/restart desktop apply must launch the staged NSIS installer or
//! replace the macOS `.app`. Copying into `updates/desktop/<ver>/current`
//! alone would leave the old Vellum binary running.
//!
//! Both paths run outside this process and relaunch Vellum when done: the
//! running app cannot overwrite its own executable, and the new build is what
//! confirms the install (`UpdateEngine::reconcile_desktop_after_restart`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::cache::{commit_candidate, mark_complete};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopInstallPlan {
    Nsis {
        installer: PathBuf,
        args: Vec<String>,
    },
    MacAppArchive {
        archive: PathBuf,
        dest_app: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopInstallLaunch {
    pub kind: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub trait DesktopRunner: Send + Sync {
    fn spawn_detached(&self, program: &Path, args: &[String]) -> Result<u32, String>;
}

pub struct HostDesktopRunner;

impl DesktopRunner for HostDesktopRunner {
    fn spawn_detached(&self, program: &Path, args: &[String]) -> Result<u32, String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group, so signals aimed at Vellum's group on the
        // way out do not stop the swap halfway.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("spawn desktop installer {}: {error}", program.display()))?;
        Ok(child.id())
    }
}

/// Replaces the `.app` once Vellum has exited, then reopens it. Arguments:
/// Vellum's pid, the verified archive, the installed `.app`. The new bundle is
/// unpacked next to the old one so both moves stay on one volume; any failure
/// puts the old bundle back, and the app that is reopened is whichever one is
/// in place. Output goes to `$TMPDIR/vellum-update.log`.
const MAC_SWAP_SCRIPT: &str = r#"set -u
pid="$1"; archive="$2"; dest="$3"
exec >>"${TMPDIR:-/tmp}/vellum-update.log" 2>&1
echo "$(date) update $dest from $archive"
waited=0
while kill -0 "$pid" 2>/dev/null; do
  waited=$((waited + 1))
  if [ "$waited" -gt 600 ]; then echo "Vellum did not exit; update skipped"; exit 1; fi
  sleep 0.1
done
work=$(mktemp -d "$(dirname "$dest")/.vellum-update.XXXXXX") || { open "$dest"; exit 1; }
new=""
if tar -xzf "$archive" -C "$work"; then
  new=$(find "$work" -maxdepth 1 -name '*.app' -type d | head -n 1)
fi
if [ -n "$new" ] && mv "$dest" "$work/previous.app"; then
  if ! mv "$new" "$dest"; then
    echo "swap failed; restoring the previous app"
    mv "$work/previous.app" "$dest"
  fi
else
  echo "archive did not yield an app; keeping the installed one"
fi
rm -rf "$work"
open "$dest"
"#;

pub fn install_dir_from_exe(current_exe: &Path) -> PathBuf {
    let mut current = current_exe.to_path_buf();
    if let Some(name) = current.file_name().and_then(|name| name.to_str()) {
        if name.ends_with(".app") {
            return current;
        }
    }
    while let Some(parent) = current.parent() {
        if parent
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            return parent.to_path_buf();
        }
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    current_exe.parent().unwrap_or(current_exe).to_path_buf()
}

pub fn plan_desktop_install(
    staged: &Path,
    current_exe: &Path,
) -> Result<DesktopInstallPlan, String> {
    let name = staged
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();
    if name.ends_with(".exe") || name.contains("nsis") {
        let install_dir = install_dir_from_exe(current_exe);
        return Ok(DesktopInstallPlan::Nsis {
            installer: staged.to_path_buf(),
            // `/S` also makes the Tauri NSIS template stop a still-running
            // Vellum instead of prompting; `/R` relaunches it after install.
            args: vec![
                "/S".into(),
                "/UPDATE".into(),
                "/R".into(),
                format!("/D={}", install_dir.display()),
            ],
        });
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.contains(".app.") {
        let dest_app = install_dir_from_exe(current_exe);
        // A build run outside a bundle has no `.app` to replace; the swap
        // would otherwise move whatever directory holds the executable.
        if !dest_app
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            return Err(format!(
                "Vellum is not running from an .app bundle ({}); refusing to replace it",
                dest_app.display()
            ));
        }
        return Ok(DesktopInstallPlan::MacAppArchive {
            archive: staged.to_path_buf(),
            dest_app,
        });
    }
    Err(format!(
        "staged desktop asset {name} is not an NSIS installer or macOS app archive; refusing to mark applied"
    ))
}

pub fn run_desktop_install(
    plan: &DesktopInstallPlan,
    runner: &dyn DesktopRunner,
) -> Result<DesktopInstallLaunch, String> {
    match plan {
        DesktopInstallPlan::Nsis { installer, args } => {
            let pid = runner.spawn_detached(installer, args)?;
            let _ = pid;
            Ok(DesktopInstallLaunch {
                kind: "nsis",
                program: installer.clone(),
                args: args.clone(),
            })
        }
        DesktopInstallPlan::MacAppArchive { archive, dest_app } => {
            let program = PathBuf::from("/bin/sh");
            let args = vec![
                "-c".to_string(),
                MAC_SWAP_SCRIPT.to_string(),
                "vellum-update".to_string(),
                std::process::id().to_string(),
                archive.display().to_string(),
                dest_app.display().to_string(),
            ];
            runner.spawn_detached(&program, &args)?;
            Ok(DesktopInstallLaunch {
                kind: "macApp",
                program,
                args,
            })
        }
    }
}

/// Versioned tree switch plus the real installer/app replace. This is the
/// function `AppApplyExecutor` and `FsApplyExecutor` both call.
pub fn commit_and_launch_desktop(
    root: &Path,
    staged: &Path,
    version: &str,
    current_exe: &Path,
    runner: &dyn DesktopRunner,
) -> Result<DesktopInstallLaunch, String> {
    let slot = root.join("updates").join("desktop").join(version);
    let candidate = slot.join("candidate");
    std::fs::create_dir_all(&candidate).map_err(|error| error.to_string())?;
    if staged.is_file() {
        let name = staged
            .file_name()
            .ok_or("staged desktop asset has no name")?;
        std::fs::copy(staged, candidate.join(name)).map_err(|error| error.to_string())?;
    }
    mark_complete(&candidate).map_err(|error| error.to_string())?;
    commit_candidate(&slot).map_err(|error| error.to_string())?;
    let installed = slot.join("current").join(
        staged
            .file_name()
            .ok_or("staged desktop asset has no name")?,
    );
    let source = if installed.is_file() {
        &installed
    } else {
        staged
    };
    let plan = plan_desktop_install(source, current_exe)?;
    run_desktop_install(&plan, runner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingRunner {
        spawns: Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                spawns: Mutex::new(Vec::new()),
            }
        }
    }

    impl DesktopRunner for RecordingRunner {
        fn spawn_detached(&self, program: &Path, args: &[String]) -> Result<u32, String> {
            self.spawns
                .lock()
                .expect("spawns")
                .push((program.to_path_buf(), args.to_vec()));
            Ok(7)
        }
    }

    #[test]
    fn nsis_plan_uses_silent_update_relaunch_and_install_dir() {
        let plan = plan_desktop_install(
            Path::new("C:/cache/Vellum_0.3.0_x64-setup.exe"),
            Path::new("C:/Program Files/Vellum/vellum-proxy-desktop.exe"),
        )
        .unwrap();
        match plan {
            DesktopInstallPlan::Nsis { args, .. } => {
                assert!(args.iter().any(|arg| arg == "/S"));
                assert!(args.iter().any(|arg| arg == "/UPDATE"));
                assert!(
                    args.iter().any(|arg| arg == "/R"),
                    "without /R a silent install leaves Vellum closed"
                );
                // NSIS requires /D to be the last argument.
                assert!(args.last().unwrap().starts_with("/D="));
            }
            other => panic!("expected NSIS, got {other:?}"),
        }
    }

    #[test]
    fn mac_archive_outside_an_app_bundle_is_refused() {
        let err = plan_desktop_install(
            Path::new("/tmp/Vellum.app.tar.gz"),
            Path::new("/Users/dev/vellum/target/debug/vellum-proxy-desktop"),
        )
        .unwrap_err();
        assert!(err.contains("not running from an .app bundle"));
    }

    #[test]
    fn payload_bin_is_not_an_installer() {
        let err = plan_desktop_install(Path::new("/tmp/payload.bin"), Path::new("/tmp/vellum"))
            .unwrap_err();
        assert!(err.contains("refusing to mark applied"));
    }

    #[test]
    fn commit_and_launch_spawns_nsis_not_only_copies() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("Vellum_0.3.0_x64-setup.exe");
        std::fs::write(&staged, b"nsis").unwrap();
        let runner = RecordingRunner::new();
        let launch = commit_and_launch_desktop(
            dir.path(),
            &staged,
            "0.3.0",
            Path::new("C:/Program Files/Vellum/vellum-proxy-desktop.exe"),
            &runner,
        )
        .unwrap();
        assert_eq!(launch.kind, "nsis");
        let current = dir
            .path()
            .join("updates/desktop/0.3.0/current/Vellum_0.3.0_x64-setup.exe");
        assert!(current.exists());
        let spawns = runner.spawns.lock().unwrap();
        assert_eq!(
            spawns.len(),
            1,
            "close-time apply must exec the NSIS installer"
        );
        assert!(spawns[0].1.iter().any(|arg| arg == "/S"));
        assert!(spawns[0].1.iter().any(|arg| arg == "/UPDATE"));
    }

    #[test]
    fn macos_archive_hands_the_swap_to_a_detached_script() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("Vellum.app.tar.gz");
        std::fs::write(&staged, b"tar").unwrap();
        let runner = RecordingRunner::new();
        let launch = commit_and_launch_desktop(
            dir.path(),
            &staged,
            "0.3.0",
            Path::new("/Applications/Vellum.app/Contents/MacOS/vellum-proxy-desktop"),
            &runner,
        )
        .unwrap();
        assert_eq!(launch.kind, "macApp");
        let spawns = runner.spawns.lock().unwrap();
        assert_eq!(
            spawns.len(),
            1,
            "the running app must not unpack over itself"
        );
        let (program, args) = &spawns[0];
        assert_eq!(program, &PathBuf::from("/bin/sh"));
        assert_eq!(args[0], "-c");
        assert_eq!(args[3], std::process::id().to_string());
        assert!(args[4].ends_with("Vellum.app.tar.gz"));
        assert_eq!(args[5], "/Applications/Vellum.app");
    }
}
