//! Close/restart desktop apply must launch the staged NSIS installer or
//! replace the macOS `.app`. Copying into `updates/desktop/<ver>/current`
//! alone would leave the old Vellum binary running.

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
    fn extract_tar_gz(&self, archive: &Path, dest_app: &Path) -> Result<(), String>;
}

pub struct HostDesktopRunner;

impl DesktopRunner for HostDesktopRunner {
    fn spawn_detached(&self, program: &Path, args: &[String]) -> Result<u32, String> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("spawn desktop installer {}: {error}", program.display()))?;
        Ok(child.id())
    }

    fn extract_tar_gz(&self, archive: &Path, dest_app: &Path) -> Result<(), String> {
        let parent = dest_app
            .parent()
            .ok_or_else(|| "macOS app has no parent directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let file = std::fs::File::open(archive).map_err(|error| error.to_string())?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(decoder);
        tar.unpack(parent).map_err(|error| error.to_string())?;
        if !dest_app.exists() {
            return Err(format!(
                "extracted archive did not produce {}",
                dest_app.display()
            ));
        }
        Ok(())
    }
}

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
            args: vec![
                "/S".into(),
                "/UPDATE".into(),
                format!("/D={}", install_dir.display()),
            ],
        });
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.contains(".app.") {
        return Ok(DesktopInstallPlan::MacAppArchive {
            archive: staged.to_path_buf(),
            dest_app: install_dir_from_exe(current_exe),
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
            runner.extract_tar_gz(archive, dest_app)?;
            Ok(DesktopInstallLaunch {
                kind: "macApp",
                program: archive.clone(),
                args: vec![dest_app.display().to_string()],
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
        extracts: Mutex<Vec<(PathBuf, PathBuf)>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                spawns: Mutex::new(Vec::new()),
                extracts: Mutex::new(Vec::new()),
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

        fn extract_tar_gz(&self, archive: &Path, dest_app: &Path) -> Result<(), String> {
            self.extracts
                .lock()
                .expect("extracts")
                .push((archive.to_path_buf(), dest_app.to_path_buf()));
            Ok(())
        }
    }

    #[test]
    fn nsis_plan_uses_silent_update_and_install_dir() {
        let plan = plan_desktop_install(
            Path::new("C:/cache/Vellum_0.3.0_x64-setup.exe"),
            Path::new("C:/Program Files/Vellum/vellum-proxy-desktop.exe"),
        )
        .unwrap();
        match plan {
            DesktopInstallPlan::Nsis { args, .. } => {
                assert!(args.iter().any(|arg| arg == "/S"));
                assert!(args.iter().any(|arg| arg == "/UPDATE"));
                assert!(args.iter().any(|arg| arg.starts_with("/D=")));
            }
            other => panic!("expected NSIS, got {other:?}"),
        }
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
    fn macos_archive_replaces_app_bundle() {
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
        let extracts = runner.extracts.lock().unwrap();
        assert_eq!(extracts.len(), 1);
        assert_eq!(extracts[0].1, PathBuf::from("/Applications/Vellum.app"));
    }
}
