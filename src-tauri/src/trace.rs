//! Optional JSON tracing subscriber for offline runtime diagnosis.
//!
//! Desktop installs only `tauri_plugin_log` (which consumes the `log` crate),
//! so the shared runtime's `tracing::info!` events are otherwise dropped. This
//! module installs a JSONL subscriber gated by `VELLUM_TRACE=1` so a crash's
//! trace can be matched to the boot record in [`crate::boot`].

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

const TRACE_ENV: &str = "VELLUM_TRACE";

/// Install the JSONL subscriber when the flag is set. Idempotent: if a global
/// subscriber is already installed, `try_init` fails and we degrade to a
/// warning without affecting startup.
pub fn install(data_root: &Path) {
    if !enabled() {
        return;
    }
    let dir = data_root.join("diagnostics");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("[Trace] cannot create diagnostics directory: {error}");
        return;
    }
    let boot_count = crate::boot::load(data_root)
        .ok()
        .flatten()
        .map(|boot| boot.boot_count)
        .unwrap_or(0);
    let path = dir.join(format!("trace-{boot_count}.jsonl"));
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            log::warn!("[Trace] cannot open trace file {}: {error}", path.display());
            return;
        }
    };
    let writer = FileWriter {
        file: Arc::new(Mutex::new(file)),
    };
    if let Err(error) = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .try_init()
    {
        log::warn!("[Trace] tracing subscriber not installed: {error}");
    }
}

/// Whether JSONL tracing is enabled for this process.
pub fn enabled() -> bool {
    std::env::var(TRACE_ENV).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// A `MakeWriter` that serializes writes through a shared file handle. It
/// returns an owned clone per write, so every tracing worker can append to the
/// same JSONL file without borrowing beyond the write.
#[derive(Clone)]
struct FileWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
