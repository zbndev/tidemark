#![allow(unsafe_code)]
//! One desktop client per session (Windows).
//!
//! Linux single-instance activation uses the session bus, which is absent on Windows,
//! so every Start launch would open a
//! second window with a second tray icon. A named session-local mutex is the guard
//! instead: the kernel releases it when the holder dies, so a crashed client never locks
//! the next one out. A second instance forwards activation through the daemon —
//! `RequestActivate` fans out to the running peer — and exits.
//!
//! This module joins the daemon's lifecycle as a locally-audited `unsafe` island: the
//! generated Win32 bindings are unsafe functions, its public surface is entirely safe,
//! and raw handles never escape it.

use std::io;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, GetLastError, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::HSTRING;

/// The session-local mutex naming the running client. `Local\` scopes it to this logon
/// session, matching the session-bus uniqueness this replaces; the name differs from the
/// daemon's (`...Daemon`, see tidemarkd's lifecycle) because the two are independent
/// singletons.
const CLIENT_MUTEX: &str = r"Local\io.github.zbndev.Tidemark.Client";

/// A restarted client must not mistake its predecessor for an ordinary second launch.
/// Wait before creating the window or contacting the daemon: the predecessor still
/// holds both the singleton and the kill-on-close job of any daemon it spawned.
pub fn wait_for_restart() -> io::Result<()> {
    let Some(parent) = std::env::var_os(crate::update::RESTART_PARENT) else {
        return Ok(());
    };
    // Consume the handoff so unrelated child processes cannot inherit a stale PID.
    // SAFETY: Windows environment mutation is thread-safe, including after logging
    // has been initialised. This module is only compiled on Windows.
    unsafe { std::env::remove_var(crate::update::RESTART_PARENT) };
    let pid = parent
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0 && *pid != std::process::id())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid restart parent PID"))?;
    wait_for_exit(pid, 30_000)
}

fn wait_for_exit(pid: u32, timeout_ms: u32) -> io::Result<()> {
    // SAFETY: request only wait access to the process named by the launch handoff. The
    // handle is not inherited and is closed after the wait, including on failure.
    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => handle,
        // The predecessor can exit before this process reaches OpenProcess. Windows
        // reports a PID that no longer exists as ERROR_INVALID_PARAMETER.
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(()),
        Err(error) => return Err(io::Error::other(error)),
    };
    // SAFETY: the process handle is valid and remains open for the entire bounded wait.
    let result = match unsafe { WaitForSingleObject(handle, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the previous Tidemark client did not exit",
        )),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!(
            "unexpected restart wait result: {}",
            other.0
        ))),
    };
    // SAFETY: this function owns the handle, and the wait has finished.
    let _ = unsafe { CloseHandle(handle) };
    result
}

/// Holds the client singleton mutex for as long as the process runs.
///
/// Acquire it before the window exists: a second instance must be gone before it can open
/// a window. Dropping the guard releases (and closes) the handle; if the process dies
/// without dropping it, the kernel closes the handle anyway.
pub struct Guard {
    handle: HANDLE,
}

impl Guard {
    /// Takes the per-session client mutex. `Ok(None)` means another client of this
    /// session already holds it: forward activation and exit instead of opening a
    /// second window.
    pub fn acquire() -> Result<Option<Self>, windows::core::Error> {
        Self::acquire_named(CLIENT_MUTEX)
    }

    fn acquire_named(name: &str) -> Result<Option<Self>, windows::core::Error> {
        let name = HSTRING::from(name);
        // SAFETY: `name` is a valid nul-terminated HSTRING borrowed for the call; the
        // returned handle is owned by us and closed in `Drop`.
        let handle = unsafe { CreateMutexW(None, false, &name) }?;
        // SAFETY: no parameters; reading the thread's last error.
        let last = unsafe { GetLastError() };
        // `CreateMutexW` succeeds even when the mutex already exists; the reason it
        // succeeded is only visible through the last error.
        // These are Windows error-code values; their numeric representation is the
        // platform contract.
        if last.0 == ERROR_ALREADY_EXISTS.0 {
            // SAFETY: the duplicate handle is ours and nothing waits on it.
            let _ = unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is the mutex this guard owns.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Guard").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Re-executes one headless helper, never the desktop client. Its mutex has a test
    /// name so running these tests cannot affect an installed client's singleton.
    #[derive(Debug)]
    struct ClientProcess {
        child: Child,
        _stdout: BufReader<std::process::ChildStdout>,
        name: String,
    }

    impl ClientProcess {
        fn start(label: &str) -> Self {
            let name = format!(
                r"Local\Tidemark.Restart.Test.{}.{label}",
                std::process::id()
            );
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "single_instance::tests::hold_client",
                    "--ignored",
                    "--nocapture",
                ])
                .env("TIDEMARK_TEST_CLIENT_MUTEX", &name)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("the headless client starts");
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert_ne!(
                    stdout.read_line(&mut line).unwrap(),
                    0,
                    "client never ready"
                );
                if line.contains("TIDEMARK_CLIENT_READY") {
                    break;
                }
                line.clear();
            }
            Self {
                child,
                _stdout: stdout,
                name,
            }
        }

        fn exit(&mut self) {
            drop(self.child.stdin.take());
            assert!(self.child.wait().unwrap().success());
        }
    }

    impl Drop for ClientProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    #[ignore = "headless subprocess helper for the restart tests"]
    fn hold_client() {
        let Some(name) = std::env::var_os("TIDEMARK_TEST_CLIENT_MUTEX") else {
            return;
        };
        let _guard = Guard::acquire_named(name.to_str().unwrap())
            .unwrap()
            .expect("the helper owns its singleton");
        println!("TIDEMARK_CLIENT_READY");
        io::stdout().flush().unwrap();
        let _ = io::stdin().read(&mut [0]).unwrap();
    }

    #[test]
    fn a_restart_waits_until_the_old_client_releases_its_singleton() {
        let mut client = ClientProcess::start("handoff");
        assert!(Guard::acquire_named(&client.name).unwrap().is_none());
        assert_eq!(
            wait_for_exit(client.child.id(), 0).unwrap_err().kind(),
            io::ErrorKind::TimedOut,
            "a running predecessor is not ready for handoff"
        );

        let pid = client.child.id();
        let (done, result) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            done.send(wait_for_exit(pid, 10_000)).unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        client.exit();
        result
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        waiter.join().unwrap();
        assert!(
            Guard::acquire_named(&client.name).unwrap().is_some(),
            "the successor can claim the singleton after the wait"
        );
    }

    #[test]
    fn a_predecessor_that_already_exited_needs_no_wait() {
        let mut client = ClientProcess::start("exited");
        let pid = client.child.id();
        client.exit();
        // Close our process handle too: the restart can arrive before OpenProcess,
        // while the object still exists, or after it has been destroyed.
        wait_for_exit(pid, 0).unwrap();
        drop(client);
        wait_for_exit(pid, 0).unwrap();
    }
}
