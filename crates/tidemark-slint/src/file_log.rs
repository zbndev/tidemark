//! File logging for the Windows client.
//!
//! The client links the GUI subsystem, so stderr goes nowhere: without this, every
//! client-side failure is invisible anywhere. The log lives at
//! `%LOCALAPPDATA%\tidemark\logs\ui.log`; when the directory cannot be created the
//! client falls back to stderr-only rather than refusing to start.

use std::sync::{Arc, Mutex};

/// Where one event goes: the log file when it opened, stderr otherwise. A single
/// static type so the subscriber builder needs no per-platform shape.
#[derive(Debug, Clone)]
pub enum Sink {
    File(FileSink),
    Stderr,
}

/// Shareable handle to the open log file.
#[derive(Debug, Clone)]
pub struct FileSink {
    file: Arc<Mutex<std::fs::File>>,
}

/// Opens `%LOCALAPPDATA%\tidemark\logs\ui.log` for appending. `None` means logging
/// stays stderr-only.
pub fn init() -> Option<FileSink> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)?
        .join("tidemark")
        .join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ui.log"))
        .ok()?;
    Some(FileSink {
        file: Arc::new(Mutex::new(file)),
    })
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Box<dyn std::io::Write + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match self {
            Self::File(sink) => Box::new(sink.make_writer()),
            Self::Stderr => Box::new(std::io::stderr()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileSink {
    type Writer = FileGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let file = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        FileGuard { file }
    }
}

/// A locked file handle completing one event write.
pub struct FileGuard<'a> {
    file: std::sync::MutexGuard<'a, std::fs::File>,
}

impl std::io::Write for FileGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        <std::fs::File as std::io::Write>::write(&mut self.file, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        <std::fs::File as std::io::Write>::flush(&mut self.file)
    }
}
