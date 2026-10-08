#![allow(unsafe_code)]
//! Installer-only process control. Every process handle is bound to an exact image path.
//! Raw Win32 handles stay in this module and are closed on every exit path.

use std::{io, path::Path, time::Duration};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, GetLastError,
    HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Networking::WinSock::{
    SIO_AF_UNIX_GETPEERPID, SOCKET, SOCKET_ERROR, WSAGetLastError, WSAIoctl,
};
use windows::Win32::Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenMutexW, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, HSTRING, PWSTR};

#[derive(Debug)]
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this module owns the handle; it is never closed elsewhere.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Owned event handle. The integer representation allows moving ownership to a waiter.
#[derive(Debug)]
pub struct StopEvent(usize);

pub fn stop_event_name(prefix: &str, pid: u32) -> String {
    format!("{prefix}.{pid}")
}
impl StopEvent {
    pub fn new(name: &str) -> io::Result<Self> {
        // SAFETY: no inherited handle, valid NUL-terminated name, manual-reset event.
        let handle = unsafe { CreateEventW(None, true, false, &HSTRING::from(name)) }
            .map_err(io::Error::other)?;
        Ok(Self(handle.0 as usize))
    }
    pub fn signal(&self) -> io::Result<()> {
        // SAFETY: this object keeps the valid event handle alive.
        unsafe { SetEvent(HANDLE(self.0 as _)) }.map_err(io::Error::other)
    }
    pub fn wait(&self, timeout_ms: u32) -> io::Result<bool> {
        // SAFETY: valid owned handle, bounded wait unless explicitly INFINITE.
        match unsafe { WaitForSingleObject(HANDLE(self.0 as _), timeout_ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
impl Drop for StopEvent {
    fn drop(&mut self) {
        // SAFETY: one owner, last use completed before drop.
        let _ = unsafe { CloseHandle(HANDLE(self.0 as _)) };
    }
}

/// Fail closed on access errors; only a missing object means no installation is active.
pub fn active() -> io::Result<bool> {
    // SAFETY: read-only open of a named mutex; an opened handle is immediately closed.
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            &HSTRING::from(tidemark_types::ids::INSTALLER_MUTEX),
        )
    } {
        Ok(handle) => {
            drop(OwnedHandle(handle));
            Ok(true)
        }
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(io::Error::other(error)),
    }
}

/// A recovery operation keeps new starts out until files and startup state are restored.
#[derive(Debug)]
pub struct MaintenanceGuard {
    _handle: OwnedHandle,
}
impl MaintenanceGuard {
    pub fn acquire() -> io::Result<Self> {
        Self::named(tidemark_types::ids::INSTALLER_MUTEX)
    }
    fn named(name: &str) -> io::Result<Self> {
        // SAFETY: owned non-inherited mutex; its existence is the startup gate.
        let handle =
            unsafe { CreateMutexW(None, false, &HSTRING::from(name)) }.map_err(io::Error::other)?;
        let existing = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle(handle);
        if existing {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another installation or recovery is active",
            ));
        }
        Ok(Self { _handle: handle })
    }
}

#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    pub pid: u32,
    pub parent_pid: u32,
    pub ui: bool,
    pub visible: bool,
}
impl Process {
    pub fn signal_stop(&self, prefix: &str) -> io::Result<()> {
        if !self.wait(Duration::ZERO)? {
            StopEvent::new(&stop_event_name(prefix, self.pid))?.signal()?;
        }
        Ok(())
    }
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        // SAFETY: the original process handle remains open, so PID reuse is irrelevant.
        match unsafe {
            WaitForSingleObject(
                self.handle.0,
                timeout.as_millis().min(u32::MAX as u128) as u32,
            )
        } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn force_stop(&self) -> io::Result<()> {
        if self.wait(Duration::ZERO)? {
            return Ok(());
        }
        // Acquire termination rights only for an already matched process. Keeping
        // its original handle alive prevents its PID from being reused here.
        // SAFETY: PID belongs to the exact installation, user and session above.
        let handle = OwnedHandle(
            unsafe { OpenProcess(PROCESS_TERMINATE, false, self.pid) }.map_err(io::Error::other)?,
        );
        // SAFETY: termination is restricted to the one-time directory migration.
        unsafe { TerminateProcess(handle.0, 0) }.map_err(io::Error::other)?;
        if self.wait(Duration::from_secs(5))? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "legacy process did not terminate",
            ))
        }
    }
}

/// Find only the installed client and daemon. Another session holding these files is an
/// installation blocker, rather than a reason to terminate that session's processes.
pub fn running(install: &Path) -> io::Result<Vec<Process>> {
    let candidates = [install.join("tidemark.exe"), install.join("tidemarkd.exe")];
    let paths: Vec<_> = candidates
        .iter()
        .map(|path| {
            if path.exists() {
                path.canonicalize().map(Some)
            } else {
                Ok(None)
            }
        })
        .collect::<io::Result<_>>()?;
    let session = session_id(std::process::id())?;
    // SAFETY: read-only process snapshot, closed through RAII.
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(io::Error::other)?,
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut result = Vec::new();
    // SAFETY: correctly sized entry and live snapshot.
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while next.is_ok() {
        let name = String::from_utf16_lossy(
            &entry.szExeFile[..entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len())],
        );
        if name.eq_ignore_ascii_case("tidemark.exe") || name.eq_ignore_ascii_case("tidemarkd.exe") {
            // SAFETY: query and wait access only; no termination right is requested
            // for another installed copy or another Windows user/session.
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    entry.th32ProcessID,
                )
            }
            .map_err(io::Error::other)?;
            let handle = OwnedHandle(handle);
            let mut buffer = vec![0u16; 32768];
            let mut length = buffer.len() as u32;
            // SAFETY: allocated writable buffer whose length is supplied to the API.
            unsafe {
                QueryFullProcessImageNameW(
                    handle.0,
                    PROCESS_NAME_WIN32,
                    PWSTR(buffer.as_mut_ptr()),
                    &mut length,
                )
            }
            .map_err(io::Error::other)?;
            let image =
                std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))
                    .canonicalize()?;
            if let Some(ui) = paths.iter().position(|path| {
                path.as_ref().is_some_and(|path| {
                    path.to_string_lossy()
                        .eq_ignore_ascii_case(&image.to_string_lossy())
                })
            }) {
                if session_id(entry.th32ProcessID)? != session || !same_user(handle.0)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Tidemark is running as another user or in another Windows session",
                    ));
                }
                result.push(Process {
                    handle,
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                    ui: ui == 0,
                    visible: visible(entry.th32ProcessID)?,
                });
            }
        }
        // SAFETY: same live snapshot and entry buffer.
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    if let Err(error) = next
        && error.code() != ERROR_NO_MORE_FILES.to_hresult()
    {
        return Err(io::Error::other(error));
    }
    Ok(result)
}

fn user_token(process: HANDLE) -> io::Result<Vec<u64>> {
    let mut token = HANDLE::default();
    // SAFETY: valid process handle, query-only token, live output handle.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(io::Error::other)?;
    let token = OwnedHandle(token);
    let mut length = 0;
    // The first call reports the required size; no output buffer is supplied.
    let query = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
    if length < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(query
            .err()
            .map(io::Error::other)
            .unwrap_or_else(|| io::Error::other("invalid token information size")));
    }
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: u64 storage is aligned for TOKEN_USER and has at least length bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .map_err(io::Error::other)?;
    Ok(buffer)
}

fn same_user(process: HANDLE) -> io::Result<bool> {
    // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, never closed.
    let own = user_token(unsafe { GetCurrentProcess() })?;
    let other = user_token(process)?;
    // SAFETY: both live aligned TOKEN_USER buffers contain API-provided SID pointers.
    let own = unsafe { &*own.as_ptr().cast::<TOKEN_USER>() };
    let other = unsafe { &*other.as_ptr().cast::<TOKEN_USER>() };
    Ok(unsafe { EqualSid(own.User.Sid, other.User.Sid) }.is_ok())
}

/// Identify the server process on the legacy AF_UNIX connection before trusting IPC.
pub fn socket_peer_pid(stream: &uds_windows::UnixStream) -> io::Result<u32> {
    use std::os::windows::io::AsRawSocket;
    let mut pid = 0u32;
    let mut written = 0;
    // SAFETY: borrowed live socket; output points to a correctly sized u32.
    let status = unsafe {
        WSAIoctl(
            SOCKET(stream.as_raw_socket() as usize),
            SIO_AF_UNIX_GETPEERPID,
            None,
            0,
            Some((&mut pid as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut written,
            None,
            None,
        )
    };
    if status == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
    }
    // AF_UNIX can return a valid PID with a zero reported byte count. Trust the
    // successful operation and nonzero ULONG output, as zbus's Windows transport does.
    if pid == 0 {
        return Err(io::Error::other("invalid AF_UNIX peer PID"));
    }
    Ok(pid)
}

fn session_id(pid: u32) -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: output points to an initialized stack variable.
    unsafe { ProcessIdToSessionId(pid, &mut session) }.map_err(io::Error::other)?;
    Ok(session)
}

fn visible(pid: u32) -> io::Result<bool> {
    struct Search {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn visit(
        window: windows::Win32::Foundation::HWND,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: EnumWindows calls synchronously with the live Search below.
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut pid = 0;
        // SAFETY: valid enumerated window, writable output variable.
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid == search.pid && unsafe { IsWindowVisible(window) }.as_bool() {
            search.found = true;
        }
        BOOL(1)
    }
    let mut search = Search { pid, found: false };
    // SAFETY: callback uses the stack pointer only during this synchronous call.
    unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) }
        .map_err(io::Error::other)?;
    Ok(search.found)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_stop_event_wakes_a_waiter_and_is_not_initially_signalled() {
        let name = format!("Local\\Tidemark.StopTest.{}", std::process::id());
        let waiter = StopEvent::new(&name).unwrap();
        assert!(!waiter.wait(0).unwrap());
        let requester = StopEvent::new(&name).unwrap();
        requester.signal().unwrap();
        assert!(waiter.wait(1000).unwrap());
    }

    #[test]
    fn stopping_one_pid_does_not_signal_another_process_event() {
        let prefix = format!("Local\\Tidemark.StopIsolationTest.{}", std::process::id());
        let target = StopEvent::new(&stop_event_name(&prefix, 100)).unwrap();
        let other = StopEvent::new(&stop_event_name(&prefix, 101)).unwrap();
        target.signal().unwrap();
        assert!(target.wait(0).unwrap());
        assert!(!other.wait(0).unwrap());
    }

    #[test]
    fn recovery_gate_is_exclusive_and_released_on_failure() {
        let name = format!("Local\\Tidemark.RecoveryGateTest.{}", std::process::id());
        let first = MaintenanceGuard::named(&name).unwrap();
        assert_eq!(
            MaintenanceGuard::named(&name).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert!(MaintenanceGuard::named(&name).is_ok());
    }

    #[test]
    fn process_identity_matches_the_current_user_token() {
        assert!(same_user(unsafe { GetCurrentProcess() }).unwrap());
    }
}
