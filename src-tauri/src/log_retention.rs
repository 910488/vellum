//! Bounded Desktop log retention, including the plugin's collision backups.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri_plugin_log::{Builder, RotationStrategy};

const MAX_FILE_BYTES: u128 = 10 * 1024 * 1024;
// KeepSome counts archives only; the active Vellum.log is the fifth file.
const MAX_ARCHIVES: usize = 4;

fn policy() -> (u128, RotationStrategy, log::LevelFilter) {
    (
        MAX_FILE_BYTES,
        RotationStrategy::KeepSome(MAX_ARCHIVES),
        log::LevelFilter::Info,
    )
}

pub(crate) fn builder() -> Builder {
    let (bytes, rotation, level) = policy();
    Builder::new()
        .max_file_size(bytes)
        .rotation_strategy(rotation)
        .level(level)
}

pub(crate) fn spawn_archive_cleanup(root: PathBuf) {
    tauri::async_runtime::spawn(async move {
        loop {
            let root = root.clone();
            match tauri::async_runtime::spawn_blocking(move || prune_archives(&root)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("[Logs] cannot prune Desktop log archives: {error}"),
                Err(error) => log::warn!("[Logs] archive cleanup worker failed: {error}"),
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

fn archive_name(name: &str) -> bool {
    let Some(date) = name.strip_prefix("Vellum_").and_then(|name| {
        name.strip_suffix(".log.bak")
            .or_else(|| name.strip_suffix(".log"))
    }) else {
        return false;
    };
    date.len() == 19
        && date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => byte == b'-',
            10 => byte == b'_',
            _ => byte.is_ascii_digit(),
        })
}

fn prune_archives(root: &Path) -> std::io::Result<()> {
    let mut backups = Vec::new();
    let mut normal_archives = 0usize;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        // Never follow links or remove the active log or another log family.
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_file() && archive_name(&name) {
            if name.ends_with(".bak") {
                backups.push(entry.path());
            } else {
                normal_archives += 1;
            }
        }
    }
    backups.sort();
    let backup_budget = MAX_ARCHIVES.saturating_sub(normal_archives);
    let excess = backups.len().saturating_sub(backup_budget);
    for path in backups.into_iter().take(excess) {
        // Leave normal archive deletion to the plugin to avoid racing its
        // rotation cleanup. KeepSome ignores these collision backups entirely.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_log_policy_retains_five_ten_mib_files_at_info() {
        let (bytes, rotation, level) = policy();
        assert_eq!(bytes, 10 * 1024 * 1024);
        assert!(matches!(rotation, RotationStrategy::KeepSome(4)));
        assert_eq!(level, log::LevelFilter::Info);
    }

    #[test]
    fn oldest_archives_and_collision_backups_share_a_bounded_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in [
            "Vellum.log",
            "Other_2026-10-12_00-00-00.log",
            "Vellum_notes.log",
        ] {
            std::fs::write(root.join(name), "untouched").unwrap();
        }
        for second in 0..4 {
            for suffix in [".log", ".log.bak"] {
                std::fs::write(
                    root.join(format!("Vellum_2026-10-12_00-00-0{second}{suffix}")),
                    "archive",
                )
                .unwrap();
            }
        }
        prune_archives(root).unwrap();
        for second in 0..4 {
            for suffix in [".log", ".log.bak"] {
                assert_eq!(
                    root.join(format!("Vellum_2026-10-12_00-00-0{second}{suffix}"))
                        .exists(),
                    suffix == ".log"
                );
            }
        }
        for name in [
            "Vellum.log",
            "Other_2026-10-12_00-00-00.log",
            "Vellum_notes.log",
        ] {
            assert_eq!(
                std::fs::read_to_string(root.join(name)).unwrap(),
                "untouched"
            );
        }
        prune_archives(root).unwrap();
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 7);

        // Backups can use spare archive slots, with the oldest removed first.
        for second in 0..2 {
            std::fs::remove_file(root.join(format!("Vellum_2026-10-12_00-00-0{second}.log")))
                .unwrap();
        }
        for second in 0..4 {
            std::fs::write(
                root.join(format!("Vellum_2026-10-12_00-00-0{second}.log.bak")),
                "archive",
            )
            .unwrap();
        }
        prune_archives(root).unwrap();
        for second in 0..4 {
            assert_eq!(
                root.join(format!("Vellum_2026-10-12_00-00-0{second}.log.bak"))
                    .exists(),
                second >= 2
            );
        }
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 7);
    }
}
