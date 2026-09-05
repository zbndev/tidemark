#![allow(unsafe_code)]
//! One desktop client per session (Windows).
//!
//! GApplication's single-instance rides the session bus, and there is none on Windows
//! (GIO logs "win32 session dbus binary not found"), so every Start launch would open a
//! second window with a second tray icon. A named session-local mutex is the guard
//! instead: the kernel releases it when the holder dies, so a crashed client never locks
//! the next one out. A second instance forwards activation through the daemon —
//! `RequestActivate` fans out to the running peer — and exits.
//!
//! This module joins the daemon's lifecycle as a locally-audited `unsafe` island: the
//! generated Win32 bindings are unsafe functions, its public surface is entirely safe,
//! and raw handles never escape it.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

/// The session-local mutex naming the running client. `Local\` scopes it to this logon
/// session, matching the session-bus uniqueness this replaces; the name differs from the
/// daemon's (`...Daemon`, see tidemarkd's lifecycle) because the two are independent
/// singletons.
const CLIENT_MUTEX: &str = r"Local\io.github.zbndev.Tidemark.Client";

/// Holds the client singleton mutex for as long as the process runs.
///
/// Acquire it before GTK initialises: a second instance must be gone before it can open
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
        let name = HSTRING::from(CLIENT_MUTEX);
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
