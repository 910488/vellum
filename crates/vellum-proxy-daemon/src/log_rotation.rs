//! Copy/truncate keeps systemd/launchd's open stdout inode valid. One bounded
//! archive per stream; no logrotate, system journal, or separate service needed.
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const LIMIT: u64 = 1024 * 1024;

fn rotate(path: &Path, limit: u64) -> std::io::Result<()> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    if len <= limit {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - limit))?;
    let mut tail = vec![];
    (&mut file).take(limit).read_to_end(&mut tail)?;
    let mut archive = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path.with_extension("log.1"))?;
    archive.write_all(&tail)?;
    archive.sync_data()?;
    // Concurrent output between copy and truncate may be lost, as with
    // logrotate copytruncate. Never rename a file still held open by systemd.
    file.set_len(0)
}

pub fn start(root: std::path::PathBuf) {
    // Rotate synchronously too: malformed configuration can exit before the
    // async task is polled, and repeated systemd restarts must remain bounded.
    rotate_all(&root);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            rotate_all(&root);
        }
    });
}

fn rotate_all(root: &Path) {
    for name in [
        "proxy.out.log",
        "proxy.err.log",
        "proxy.launchd.out.log",
        "proxy.launchd.err.log",
    ] {
        // Best effort; a logging failure must never stop serving requests.
        let _ = rotate(&root.join("logs").join(name), LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotates_to_one_bounded_archive_without_replacing_the_live_inode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("proxy.out.log");
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(b"abcdefghijk").unwrap();
        rotate(&path, 4).unwrap();
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"hijk"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        writer.write_all(b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        writer.write_all(b"data").unwrap();
        rotate(&path, 4).unwrap();
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"data"
        );
    }

    #[tokio::test]
    async fn startup_rotation_does_not_depend_on_the_first_async_tick() {
        let temp = tempfile::tempdir().unwrap();
        let logs = temp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join("proxy.err.log");
        std::fs::write(&path, vec![b'x'; LIMIT as usize + 1]).unwrap();
        start(temp.path().into());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len(),
            LIMIT
        );
    }
}
