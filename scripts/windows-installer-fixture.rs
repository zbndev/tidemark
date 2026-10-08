//! Headless installer fixture. Build directly with rustc; never launch the GUI.
#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Handle = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn OpenMutexW(access: u32, inherit: i32, name: *const u16) -> Handle;
    fn GetLastError() -> u32;
    fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> Handle;
    fn SetEvent(handle: Handle) -> i32;
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn record(path: &Path, role: &str, action: &str, background: bool) {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let line = format!(
        "{time}|{}|{role}|{action}|background={background}\n",
        std::process::id()
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(line.as_bytes()).unwrap();
    file.sync_data().unwrap();
}

fn main() {
    let Some(log) = std::env::var_os("TIDEMARK_FIXTURE_LOG") else {
        return;
    };
    let log = Path::new(&log);
    let image = std::env::current_exe().unwrap();
    let daemon = image
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .eq_ignore_ascii_case("tidemarkd");
    let role = if daemon { "daemon" } else { "client" };
    let background = std::env::args().any(|argument| argument == "--background");
    if !cfg!(fixture_legacy) {
        let gate = wide(r"Local\io.github.zbndev.Tidemark.Maintenance");
        // SAFETY: valid terminated name; query-only handle closed immediately.
        let handle = unsafe { OpenMutexW(0x0010_0000, 0, gate.as_ptr()) };
        if !handle.is_null() {
            unsafe { CloseHandle(handle) };
            record(log, role, "blocked", background);
            return;
        }
        // Only an absent gate permits a start; access failures fail closed.
        if unsafe { GetLastError() } != 2 {
            return;
        }
    }
    let mut singleton = std::ptr::null_mut();
    if daemon {
        let mut hash = DefaultHasher::new();
        image
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_lowercase()
            .hash(&mut hash);
        let name = wide(&format!(
            r"Local\Tidemark.Fixture.Daemon.{:x}",
            hash.finish()
        ));
        // SAFETY: the mutex's lifetime prevents a client from spawning a second
        // daemon into the same fixture installation. Other directories are independent.
        singleton = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        assert!(!singleton.is_null());
        if unsafe { GetLastError() } == 183 {
            unsafe { CloseHandle(singleton) };
            record(log, role, "duplicate-suppressed", background);
            return;
        }
    }
    record(log, role, "started", background);
    let delay = if daemon {
        std::env::var("TIDEMARK_FIXTURE_READY_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    std::thread::sleep(Duration::from_millis(delay));
    let mut event = std::ptr::null_mut();
    if !cfg!(fixture_legacy) {
        let prefix = if daemon { "StopDaemon" } else { "StopClient" };
        let name = wide(&format!(
            r"Local\io.github.zbndev.Tidemark.{prefix}.{}",
            std::process::id()
        ));
        // SAFETY: a named manual-reset event and a valid terminated name.
        event = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        assert!(!event.is_null(), "fixture event creation failed");
    }
    let mut child =
        if !daemon && std::env::var("TIDEMARK_FIXTURE_SPAWN_DAEMON").as_deref() == Ok("1") {
            Some(
                Command::new(image.with_file_name("tidemarkd.exe"))
                    .creation_flags(0x0800_0000)
                    .spawn()
                    .unwrap(),
            )
        } else {
            None
        };
    let mut ready = std::ptr::null_mut();
    if daemon {
        let name = wide(&format!(
            r"Local\io.github.zbndev.Tidemark.ReadyDaemon.{}",
            std::process::id()
        ));
        // SAFETY: the daemon keeps the ready event alive until process exit,
        // matching the real daemon's readiness publication after its IPC bind.
        ready = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        assert!(!ready.is_null());
        assert_ne!(unsafe { SetEvent(ready) }, 0);
    }
    record(log, role, "ready", background);
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if !event.is_null() {
            // SAFETY: this process owns the live event for the entire wait.
            if unsafe { WaitForSingleObject(event, 100) } == 0 {
                record(log, role, "stopped", background);
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if !event.is_null() {
        unsafe { CloseHandle(event) };
    }
    if !ready.is_null() {
        unsafe { CloseHandle(ready) };
    }
    if !singleton.is_null() {
        unsafe { CloseHandle(singleton) };
    }
    if let Some(child) = child.as_mut() {
        // A correct installer stopped the daemon first. Cleanup stays bounded
        // even when the test fails and terminates this process by its exact PID.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}
